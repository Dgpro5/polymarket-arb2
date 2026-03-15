// On-chain helpers — USDC balance, allowances, ERC-1155 approval, CTF redemption, POL top-up.

use alloy::providers::ProviderBuilder;
use alloy::signers::Signer as _;
use alloy::signers::local::LocalSigner;
use anyhow::{Context, Result, anyhow};
use ethers::prelude::*;
use ethers::signers::Signer;
use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::ctf::{Client as CtfClient, types::RedeemPositionsRequest};
use polymarket_client_sdk::types::{Address as AlloyAddress, B256, U256 as AlloyU256};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::env;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::consts::{
    ANKR_API_KEY_ENV, CHAIN_ID, CONDITIONAL_TOKENS_ADDRESS, CTF_EXCHANGE_ADDRESS, GAMMA_API,
    PER_WINDOW_MAX_USD, POL_CRITICAL_THRESHOLD, POL_LOW_THRESHOLD, POL_TO_USDC_SWAP_FRACTION,
    POL_TOP_UP_USDC, SIMPLESWAP_API_KEY_ENV, USDC_E_POLYGON,
};
use crate::polymarket::TradingWallet;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SimpleSwapExchange {
    #[serde(rename = "publicId")]
    public_id: String,
    #[serde(rename = "addressFrom")]
    address_from: String,
    #[serde(rename = "amountFrom")]
    amount_from: String,
    status: String,
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Ensure both USDC allowance and ERC-1155 approval are in place.
pub async fn ensure_approvals(client: &Client, wallet: &TradingWallet) -> Result<()> {
    ensure_allowance(client, wallet, CTF_EXCHANGE_ADDRESS)
        .await
        .context("USDC allowance")?;
    ensure_ctf_token_approval(client, wallet)
        .await
        .context("ERC-1155 approval")?;
    Ok(())
}

pub async fn get_balance(client: &Client, address: &Address) -> Result<f64> {
    let rpc_url = ankr_rpc()?;
    let addr_hex = format!("{:x}", address);
    let calldata = format!("0x70a08231{:0>64}", addr_hex);

    let body = json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{ "to": USDC_E_POLYGON, "data": calldata }, "latest"],
        "id": 1
    });

    let resp = client.post(&rpc_url).json(&body).send().await?;
    let raw = resp.text().await?;
    let v: Value = serde_json::from_str(&raw)?;

    if let Some(err) = v.get("error") {
        return Err(anyhow!("eth_call error: {err}"));
    }

    let hex = v
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0")
        .trim_start_matches("0x");
    let raw_amount = u128::from_str_radix(hex, 16).unwrap_or(0);
    Ok(raw_amount as f64 / 1_000_000.0)
}

/// Scan the last 5 resolved windows (skip most recent) and redeem positions.
pub async fn redeem_prior_windows(client: &Client, private_key: &str) -> u32 {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let mut redeemed = 0u32;

    for i in 2..=6 {
        let ws = (now_secs / 300) * 300 - i * 300;
        let slug = format!("btc-updown-5m-{ws}");
        let url = format!("{GAMMA_API}/markets?slug={slug}");

        let cid = match client.get(&url).send().await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(data) => data
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|m| m.get("conditionId"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                Err(_) => None,
            },
            Err(_) => None,
        };

        if let Some(cid) = cid {
            match redeem_positions(private_key, &cid).await {
                Ok(_) => redeemed += 1,
                Err(e) => {
                    let msg = format!("{e:#}");
                    if !msg.contains("revert") && !msg.contains("insufficient") {
                        eprintln!("Redeem failed for {slug}: {e:#}");
                    }
                }
            }
        }
    }

    redeemed
}

pub async fn check_and_top_up_pol(client: &Client, wallet: &TradingWallet) -> Result<()> {
    if env::var(SIMPLESWAP_API_KEY_ENV)
        .map(|k| k.trim().is_empty())
        .unwrap_or(true)
    {
        return Ok(());
    }

    let pol = match get_pol_balance(client, &wallet.address).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("WARN: Could not check POL balance: {e:#}");
            return Ok(());
        }
    };

    if pol >= POL_LOW_THRESHOLD {
        return Ok(());
    }

    let recipient = format!("{:#x}", wallet.address);
    let exchange = match create_simpleswap_exchange(client, POL_TOP_UP_USDC, &recipient).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("WARN: SimpleSwap creation failed: {e:#}");
            return Ok(());
        }
    };

    if exchange.address_from.is_empty() {
        eprintln!("WARN: SimpleSwap returned empty deposit address");
        return Ok(());
    }

    match send_usdc_transfer(client, wallet, &exchange.address_from, POL_TOP_UP_USDC).await {
        Ok(_) => {}
        Err(e) => eprintln!("POL top-up transfer failed: {e:#}"),
    }

    Ok(())
}

// ── Allowance ────────────────────────────────────────────────────────────────

async fn get_allowance(client: &Client, owner: &Address, spender: &str) -> Result<f64> {
    let rpc_url = ankr_rpc()?;
    let owner_hex = format!("{:x}", owner);
    let spender_hex = spender.trim_start_matches("0x");
    let calldata = format!("0xdd62ed3e{:0>64}{:0>64}", owner_hex, spender_hex);

    let body = json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{ "to": USDC_E_POLYGON, "data": calldata }, "latest"],
        "id": 1
    });

    let raw = client.post(&rpc_url).json(&body).send().await?.text().await?;
    let v: Value = serde_json::from_str(&raw)?;
    if let Some(err) = v.get("error") {
        return Err(anyhow!("allowance error: {err}"));
    }
    let hex = v
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0")
        .trim_start_matches("0x");
    Ok(u128::from_str_radix(hex, 16).unwrap_or(0) as f64 / 1_000_000.0)
}

async fn ensure_allowance(
    client: &Client,
    wallet: &TradingWallet,
    spender: &str,
) -> Result<()> {
    if get_allowance(client, &wallet.address, spender).await? >= 1000.0 {
        return Ok(());
    }

    let rpc_url = ankr_rpc()?;
    let nonce = get_nonce(client, &rpc_url, &wallet.address).await?;
    let gas_price = get_gas_price(client, &rpc_url).await?;

    let sp = spender.trim_start_matches("0x");
    let cd = hex::decode(format!("095ea7b3{:0>64}{}", sp, "f".repeat(64)))?;

    use ethers::types::transaction::eip2718::TypedTransaction;
    let tx = TypedTransaction::Legacy(ethers::types::TransactionRequest {
        from: Some(wallet.address),
        to: Some(USDC_E_POLYGON.parse::<Address>().unwrap().into()),
        nonce: Some(U256::from(nonce)),
        gas: Some(U256::from(100_000u64)),
        gas_price: Some(U256::from(gas_price * 3)),
        data: Some(cd.into()),
        value: Some(U256::zero()),
        chain_id: Some(U64::from(CHAIN_ID)),
        ..Default::default()
    });

    let sig = wallet
        .wallet
        .sign_transaction(&tx)
        .await
        .map_err(|e| anyhow!("sign approve: {e}"))?;
    let raw_tx = format!("0x{}", hex::encode(tx.rlp_signed(&sig)));

    let hash = send_raw_tx(client, &rpc_url, &raw_tx).await?;
    wait_for_receipt(client, &rpc_url, &hash).await
}

// ── ERC-1155 approval ────────────────────────────────────────────────────────

async fn is_approved_for_all(
    client: &Client,
    owner: &Address,
    operator: &str,
    contract: &str,
) -> Result<bool> {
    let rpc_url = ankr_rpc()?;
    let o = format!("{:x}", owner);
    let op = operator.trim_start_matches("0x");
    let cd = format!("e985e9c5{:0>64}{:0>64}", o, op);

    let body = json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": contract, "data": format!("0x{cd}")}, "latest"],
        "id": 1
    });

    let raw = client.post(&rpc_url).json(&body).send().await?.text().await?;
    let v: Value = serde_json::from_str(&raw)?;
    if let Some(err) = v.get("error") {
        return Err(anyhow!("isApprovedForAll error: {err}"));
    }
    let hex = v
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0")
        .trim_start_matches("0x");
    Ok(u128::from_str_radix(hex, 16).unwrap_or(0) != 0)
}

async fn ensure_ctf_token_approval(client: &Client, wallet: &TradingWallet) -> Result<()> {
    if is_approved_for_all(
        client,
        &wallet.address,
        CTF_EXCHANGE_ADDRESS,
        CONDITIONAL_TOKENS_ADDRESS,
    )
    .await?
    {
        return Ok(());
    }

    eprintln!("ERC-1155 approval missing — sending setApprovalForAll…");

    let rpc_url = ankr_rpc()?;
    let nonce = get_nonce(client, &rpc_url, &wallet.address).await?;
    let gas_price = get_gas_price(client, &rpc_url).await?;

    let op = CTF_EXCHANGE_ADDRESS.trim_start_matches("0x");
    let cd = hex::decode(format!("a22cb465{:0>64}{:0>64}", op, "1"))?;

    use ethers::types::transaction::eip2718::TypedTransaction;
    let tx = TypedTransaction::Legacy(ethers::types::TransactionRequest {
        from: Some(wallet.address),
        to: Some(
            CONDITIONAL_TOKENS_ADDRESS
                .parse::<Address>()
                .unwrap()
                .into(),
        ),
        nonce: Some(U256::from(nonce)),
        gas: Some(U256::from(100_000u64)),
        gas_price: Some(U256::from(gas_price * 3)),
        data: Some(cd.into()),
        value: Some(U256::zero()),
        chain_id: Some(U64::from(CHAIN_ID)),
        ..Default::default()
    });

    let sig = wallet
        .wallet
        .sign_transaction(&tx)
        .await
        .map_err(|e| anyhow!("sign setApprovalForAll: {e}"))?;
    let raw_tx = format!("0x{}", hex::encode(tx.rlp_signed(&sig)));

    let hash = send_raw_tx(client, &rpc_url, &raw_tx).await?;
    wait_for_receipt(client, &rpc_url, &hash).await?;
    eprintln!("ERC-1155 approval set: {hash}");
    Ok(())
}

// ── CTF redemption ───────────────────────────────────────────────────────────

async fn redeem_positions(private_key: &str, condition_id_hex: &str) -> Result<()> {
    let rpc_url = ankr_rpc()?;

    let signer = LocalSigner::from_str(private_key.trim())
        .context("parse key for redeem")?
        .with_chain_id(Some(POLYGON));

    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect(&rpc_url)
        .await
        .context("connect provider")?;

    let ctf = CtfClient::new(provider, POLYGON).context("init ctf")?;

    let collateral: AlloyAddress = USDC_E_POLYGON.parse()?;
    let cid: B256 = condition_id_hex.parse()?;

    let mut req = RedeemPositionsRequest::for_binary_market(collateral, cid);
    req.index_sets = vec![AlloyU256::from(1), AlloyU256::from(2)];

    ctf.redeem_positions(&req)
        .await
        .with_context(|| format!("redeem {cid:#x}"))?;
    Ok(())
}

// ── POL balance & top-up ─────────────────────────────────────────────────────

pub async fn get_pol_balance(client: &Client, address: &Address) -> Result<f64> {
    let rpc_url = ankr_rpc()?;
    let body = json!({
        "jsonrpc": "2.0",
        "method": "eth_getBalance",
        "params": [format!("{:#x}", address), "latest"],
        "id": 1
    });
    let v: Value = client.post(&rpc_url).json(&body).send().await?.json().await?;
    if let Some(err) = v.get("error") {
        return Err(anyhow!("eth_getBalance error: {err}"));
    }
    let hex = v["result"]
        .as_str()
        .unwrap_or("0x0")
        .trim_start_matches("0x");
    Ok(u128::from_str_radix(hex, 16).unwrap_or(0) as f64 / 1e18)
}

async fn create_simpleswap_exchange(
    client: &Client,
    amount_usdc: f64,
    recipient: &str,
) -> Result<SimpleSwapExchange> {
    let api_key = env::var(SIMPLESWAP_API_KEY_ENV)?;
    let payload = json!({
        "tickerFrom": "usdcpoly", "networkFrom": "polygon",
        "tickerTo": "pol", "networkTo": "polygon",
        "amount": format!("{:.6}", amount_usdc),
        "fixed": false, "reverse": false, "customFee": null,
        "addressTo": recipient, "extraIdTo": "",
        "userRefundAddress": recipient, "userRefundExtraId": "", "rateId": ""
    });

    let data: Value = client
        .post("https://api.simpleswap.io/v3/exchanges")
        .header("x-api-key", api_key.trim())
        .header("Accept", "application/json")
        .json(&payload)
        .send()
        .await?
        .json()
        .await?;

    let obj = data.get("result").unwrap_or(&data);
    if let Some(err) = data.get("error").or_else(|| data.get("message")) {
        return Err(anyhow!("SimpleSwap error: {err}"));
    }

    Ok(SimpleSwapExchange {
        public_id: obj["publicId"].as_str().unwrap_or("").into(),
        address_from: obj["addressFrom"].as_str().unwrap_or("").into(),
        amount_from: obj["amountFrom"].as_str().unwrap_or("").into(),
        status: obj["status"].as_str().unwrap_or("").into(),
    })
}

async fn send_usdc_transfer(
    client: &Client,
    wallet: &TradingWallet,
    to_address: &str,
    amount_usdc: f64,
) -> Result<String> {
    let rpc_url = ankr_rpc()?;
    let nonce = get_nonce(client, &rpc_url, &wallet.address).await?;
    let gas_price = get_gas_price(client, &rpc_url).await?;

    let raw_amt = (amount_usdc * 1_000_000.0) as u64;
    let to = to_address.trim_start_matches("0x");
    let cd = hex::decode(format!("a9059cbb{:0>64}{:0>64x}", to, raw_amt))?;

    use ethers::types::transaction::eip2718::TypedTransaction;
    let tx = TypedTransaction::Legacy(ethers::types::TransactionRequest {
        from: Some(wallet.address),
        to: Some(USDC_E_POLYGON.parse::<Address>().unwrap().into()),
        nonce: Some(U256::from(nonce)),
        gas: Some(U256::from(80_000u64)),
        gas_price: Some(U256::from(gas_price * 3)),
        data: Some(cd.into()),
        value: Some(U256::zero()),
        chain_id: Some(U64::from(CHAIN_ID)),
        ..Default::default()
    });

    let sig = wallet
        .wallet
        .sign_transaction(&tx)
        .await
        .map_err(|e| anyhow!("sign transfer: {e}"))?;
    let raw_tx = format!("0x{}", hex::encode(tx.rlp_signed(&sig)));
    send_raw_tx(client, &rpc_url, &raw_tx).await
}

// ── Preflight balance check ──────────────────────────────────────────────

/// Check POL and USDC.e balances on startup.
/// - POL <= 5  → Discord alert + return Err to halt the bot.
/// - USDC.e == 0 or < PER_WINDOW_MAX_USD → swap 80% of POL to USDC.e.
pub async fn preflight_balance_check(client: &Client, wallet: &TradingWallet) -> Result<()> {
    let pol = get_pol_balance(client, &wallet.address)
        .await
        .context("preflight: failed to fetch POL balance")?;
    eprintln!("POL balance: {:.4}", pol);

    if pol <= POL_CRITICAL_THRESHOLD {
        crate::alerts::send_low_pol_alert(client, pol).await;
        return Err(anyhow!(
            "POL balance ({:.4}) is at or below critical threshold ({:.1}). Halting.",
            pol,
            POL_CRITICAL_THRESHOLD
        ));
    }

    let usdc = get_balance(client, &wallet.address)
        .await
        .context("preflight: failed to fetch USDC.e balance")?;
    eprintln!("USDC.e balance: ${:.4}", usdc);

    if usdc >= PER_WINDOW_MAX_USD {
        return Ok(());
    }

    // Not enough USDC.e — swap 80% of POL to USDC.e
    eprintln!(
        "USDC.e too low (${:.4}) — swapping {:.0}% of POL to USDC.e…",
        usdc,
        POL_TO_USDC_SWAP_FRACTION * 100.0
    );

    if env::var(SIMPLESWAP_API_KEY_ENV)
        .map(|k| k.trim().is_empty())
        .unwrap_or(true)
    {
        return Err(anyhow!(
            "USDC.e balance insufficient and SIMPLESWAP_API_KEY not set — cannot swap POL."
        ));
    }

    let swap_pol = pol * POL_TO_USDC_SWAP_FRACTION;
    let recipient = format!("{:#x}", wallet.address);

    let exchange = create_simpleswap_pol_to_usdc(client, swap_pol, &recipient)
        .await
        .context("preflight: SimpleSwap POL→USDC.e creation failed")?;

    if exchange.address_from.is_empty() {
        return Err(anyhow!("SimpleSwap returned empty deposit address for POL→USDC.e swap"));
    }

    eprintln!(
        "SimpleSwap exchange created (POL→USDC.e): id={}, deposit={}",
        exchange.public_id, exchange.address_from
    );

    send_pol_transfer(client, wallet, &exchange.address_from, swap_pol)
        .await
        .context("preflight: failed to send POL to SimpleSwap deposit")?;

    eprintln!("Sent {:.4} POL to SimpleSwap. USDC.e will arrive shortly.", swap_pol);
    Ok(())
}

async fn create_simpleswap_pol_to_usdc(
    client: &Client,
    amount_pol: f64,
    recipient: &str,
) -> Result<SimpleSwapExchange> {
    let api_key = env::var(SIMPLESWAP_API_KEY_ENV)?;
    let payload = json!({
        "tickerFrom": "pol", "networkFrom": "polygon",
        "tickerTo": "usdcpoly", "networkTo": "polygon",
        "amount": format!("{:.6}", amount_pol),
        "fixed": false, "reverse": false, "customFee": null,
        "addressTo": recipient, "extraIdTo": "",
        "userRefundAddress": recipient, "userRefundExtraId": "", "rateId": ""
    });

    let data: Value = client
        .post("https://api.simpleswap.io/v3/exchanges")
        .header("x-api-key", api_key.trim())
        .header("Accept", "application/json")
        .json(&payload)
        .send()
        .await?
        .json()
        .await?;

    let obj = data.get("result").unwrap_or(&data);
    if let Some(err) = data.get("error").or_else(|| data.get("message")) {
        return Err(anyhow!("SimpleSwap error: {err}"));
    }

    Ok(SimpleSwapExchange {
        public_id: obj["publicId"].as_str().unwrap_or("").into(),
        address_from: obj["addressFrom"].as_str().unwrap_or("").into(),
        amount_from: obj["amountFrom"].as_str().unwrap_or("").into(),
        status: obj["status"].as_str().unwrap_or("").into(),
    })
}

/// Send native POL to an address (e.g. SimpleSwap deposit).
async fn send_pol_transfer(
    client: &Client,
    wallet: &TradingWallet,
    to_address: &str,
    amount_pol: f64,
) -> Result<String> {
    let rpc_url = ankr_rpc()?;
    let nonce = get_nonce(client, &rpc_url, &wallet.address).await?;
    let gas_price = get_gas_price(client, &rpc_url).await?;

    // Convert POL to wei (1 POL = 1e18 wei)
    let wei = (amount_pol * 1e18) as u128;

    use ethers::types::transaction::eip2718::TypedTransaction;
    let tx = TypedTransaction::Legacy(ethers::types::TransactionRequest {
        from: Some(wallet.address),
        to: Some(to_address.parse::<Address>().context("parse deposit address")?.into()),
        nonce: Some(U256::from(nonce)),
        gas: Some(U256::from(21_000u64)),
        gas_price: Some(U256::from(gas_price * 3)),
        data: None,
        value: Some(U256::from(wei)),
        chain_id: Some(U64::from(CHAIN_ID)),
        ..Default::default()
    });

    let sig = wallet
        .wallet
        .sign_transaction(&tx)
        .await
        .map_err(|e| anyhow!("sign POL transfer: {e}"))?;
    let raw_tx = format!("0x{}", hex::encode(tx.rlp_signed(&sig)));

    let hash = send_raw_tx(client, &rpc_url, &raw_tx).await?;
    wait_for_receipt(client, &rpc_url, &hash).await?;
    eprintln!("POL transfer confirmed: {hash}");
    Ok(hash)
}

// ── Shared RPC helpers ───────────────────────────────────────────────────────

fn ankr_rpc() -> Result<String> {
    let key = env::var(ANKR_API_KEY_ENV)
        .with_context(|| format!("Missing '{ANKR_API_KEY_ENV}' in .env"))?;
    Ok(format!("https://rpc.ankr.com/polygon/{}", key.trim()))
}

async fn get_nonce(client: &Client, rpc_url: &str, address: &Address) -> Result<u64> {
    let body = json!({
        "jsonrpc": "2.0", "method": "eth_getTransactionCount",
        "params": [format!("{:#x}", address), "latest"], "id": 1
    });
    let v: Value = client.post(rpc_url).json(&body).send().await?.json().await?;
    v["result"]
        .as_str()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| anyhow!("bad nonce: {v}"))
}

async fn get_gas_price(client: &Client, rpc_url: &str) -> Result<u128> {
    let body = json!({"jsonrpc":"2.0","method":"eth_gasPrice","params":[],"id":1});
    let v: Value = client.post(rpc_url).json(&body).send().await?.json().await?;
    v["result"]
        .as_str()
        .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| anyhow!("bad gas price: {v}"))
}

async fn send_raw_tx(client: &Client, rpc_url: &str, raw_tx: &str) -> Result<String> {
    let body = json!({
        "jsonrpc": "2.0", "method": "eth_sendRawTransaction",
        "params": [raw_tx], "id": 1
    });
    let v: Value = client.post(rpc_url).json(&body).send().await?.json().await?;
    if let Some(err) = v.get("error") {
        return Err(anyhow!("tx failed: {err}"));
    }
    v["result"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("no tx hash: {v}"))
}

async fn wait_for_receipt(client: &Client, rpc_url: &str, tx_hash: &str) -> Result<()> {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let body = json!({
            "jsonrpc": "2.0", "method": "eth_getTransactionReceipt",
            "params": [tx_hash], "id": 1
        });
        let v: Value = client.post(rpc_url).json(&body).send().await?.json().await?;
        if let Some(r) = v.get("result").filter(|r| !r.is_null()) {
            return if r["status"].as_str().unwrap_or("0x0") == "0x1" {
                Ok(())
            } else {
                Err(anyhow!("Tx reverted. Check wallet has POL for gas."))
            };
        }
    }
    Err(anyhow!("Tx not confirmed within 30s: {tx_hash}"))
}
