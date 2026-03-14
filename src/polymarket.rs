// Polymarket CLOB — market discovery, WebSocket price stream, API auth, order management.
// Functions for order building/signing/submission are kept for future arb strategies.
#![allow(dead_code)]

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::URL_SAFE as BASE64};
use ethers::prelude::*;
use ethers::signers::{LocalWallet, Signer};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::chain::{CHAIN_ID, CTF_EXCHANGE_ADDRESS};

// ── Constants ────────────────────────────────────────────────────────────────

const GAMMA_API: &str = "https://gamma-api.polymarket.com";
pub(crate) const CLOB_API: &str = "https://clob.polymarket.com";
const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub slug: String,
    pub end_ts: i64,
    pub condition_id: String,
    pub asset_ids: Vec<String>,
    pub outcomes: Vec<String>,
}

/// Live Polymarket outcome prices for the current window.
#[derive(Debug)]
pub struct MarketState {
    pub market_slug: String,
    pub condition_id: String,
    pub asset_to_outcome: HashMap<String, String>,
    pub best_asks: HashMap<String, f64>,
    pub mid_prices: HashMap<String, f64>,
    pub fee_bps: u64,
}

impl MarketState {
    pub fn new_shared(market: &MarketInfo) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            market_slug: market.slug.clone(),
            condition_id: market.condition_id.clone(),
            asset_to_outcome: market
                .asset_ids
                .iter()
                .cloned()
                .zip(market.outcomes.iter().cloned())
                .collect(),
            best_asks: HashMap::new(),
            mid_prices: HashMap::new(),
            fee_bps: 1000,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct ApiCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

#[derive(Debug, Clone)]
pub struct TradingWallet {
    pub wallet: LocalWallet,
    pub address: Address,
    pub creds: ApiCredentials,
}

#[derive(Debug, Deserialize)]
pub struct OrderBookLevel {
    pub price: String,
    pub size: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct OrderBook {
    pub bids: Vec<OrderBookLevel>,
    pub asks: Vec<OrderBookLevel>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BatchOrderResult {
    pub success: bool,
    #[serde(rename = "orderID", default)]
    pub order_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(rename = "errorMsg", default)]
    pub error_msg: String,
    #[serde(rename = "takingAmount", default)]
    pub taking_amount: String,
    #[serde(rename = "makingAmount", default)]
    pub making_amount: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct OpenOrder {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub original_size: String,
    #[serde(default)]
    pub size_matched: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct PolymarketOrderStruct {
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    #[serde(rename = "tokenId")]
    pub token_id: String,
    #[serde(rename = "makerAmount")]
    pub maker_amount: String,
    #[serde(rename = "takerAmount")]
    pub taker_amount: String,
    pub side: String,
    pub expiration: String,
    pub nonce: String,
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: String,
    pub signature: String,
    #[serde(rename = "signatureType")]
    pub signature_type: u8,
}

#[derive(Debug, Serialize)]
pub struct CreateOrderRequest {
    pub order: PolymarketOrderStruct,
    pub owner: String,
    #[serde(rename = "orderType")]
    pub order_type: String,
    #[serde(rename = "deferExec")]
    pub defer_exec: bool,
}

// ── Wallet setup ─────────────────────────────────────────────────────────────

pub async fn setup_wallet(private_key: &str) -> Result<Arc<TradingWallet>> {
    let client = Client::new();

    let wallet_signer = private_key
        .trim()
        .parse::<LocalWallet>()
        .context("parse private key")?;
    let address = wallet_signer.address();
    eprintln!("Wallet: {:#x}", address);

    let creds = get_or_create_api_creds(&wallet_signer, address, &client).await?;

    Ok(Arc::new(TradingWallet {
        wallet: wallet_signer,
        address,
        creds,
    }))
}

// ── Market discovery ─────────────────────────────────────────────────────────

pub fn compute_current_slug() -> (String, i64) {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let remainder = now_secs % 300;
    let start_ts = now_secs - remainder;
    let end_ts = start_ts + 300;
    (format!("btc-updown-5m-{start_ts}"), end_ts)
}

pub async fn discover_active_btc_5m_market(client: &Client) -> Result<MarketInfo> {
    let (slug, end_ts) = compute_current_slug();

    let url = format!("{GAMMA_API}/markets?slug={slug}");
    let resp = client.get(&url).send().await.context("fetch market by slug")?;
    let data: Value = resp.json().await.context("parse market response")?;

    let market = data
        .as_array()
        .and_then(|arr| arr.first())
        .ok_or_else(|| anyhow!("Market not found for slug '{slug}'"))?;

    let mut info = parse_market_info(market)?;
    info.slug = slug;
    info.end_ts = end_ts;
    Ok(info)
}

fn parse_market_info(market: &Value) -> Result<MarketInfo> {
    let condition_id = market
        .get("conditionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing conditionId"))?
        .to_string();

    let asset_ids = parse_string_array(market.get("clobTokenIds")).context("parse clobTokenIds")?;
    let outcomes = parse_string_array(market.get("outcomes")).context("parse outcomes")?;

    if asset_ids.is_empty() || outcomes.is_empty() {
        return Err(anyhow!("empty outcomes or token ids"));
    }
    if asset_ids.len() != outcomes.len() {
        return Err(anyhow!(
            "token id count {} != outcomes {}",
            asset_ids.len(),
            outcomes.len()
        ));
    }

    Ok(MarketInfo {
        slug: String::new(),
        end_ts: 0,
        condition_id,
        asset_ids,
        outcomes,
    })
}

fn parse_string_array(value: Option<&Value>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Err(anyhow!("missing field"));
    };
    if let Some(arr) = value.as_array() {
        return Ok(arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect());
    }
    if let Some(s) = value.as_str() {
        let parsed: Vec<String> = serde_json::from_str(s).context("parse json string array")?;
        return Ok(parsed);
    }
    Err(anyhow!("unexpected field type"))
}

// ── WebSocket price stream ───────────────────────────────────────────────────

pub async fn run_market_ws(
    state: Arc<Mutex<MarketState>>,
    asset_ids: &[String],
) -> Result<()> {
    let (ws, _) = connect_async(CLOB_WS_URL).await.context("connect Polymarket WS")?;
    let (mut write, mut read) = ws.split();

    let sub = json!({ "type": "market", "assets_ids": asset_ids });
    write
        .send(Message::Text(sub.to_string()))
        .await
        .context("send subscribe")?;

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                match text.as_str() {
                    "ping" => {
                        write.send(Message::Text("pong".to_string())).await.ok();
                        continue;
                    }
                    "pong" => continue,
                    "NO NEW ASSETS" => return Err(anyhow!("NO NEW ASSETS")),
                    _ => {}
                }
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    handle_clob_message(&state, &value).await;
                }
            }
            Ok(Message::Ping(data)) => {
                write.send(Message::Pong(data)).await.ok();
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => return Err(anyhow!("Polymarket WS error: {e}")),
        }
    }

    Ok(())
}

async fn handle_clob_message(state: &Arc<Mutex<MarketState>>, value: &Value) {
    if let Some(arr) = value.as_array() {
        for item in arr {
            handle_single(state, item).await;
        }
        return;
    }
    if let Some(data) = value.get("data") {
        if let Some(arr) = data.as_array() {
            for item in arr {
                handle_single(state, item).await;
            }
            return;
        }
        if data.is_object() {
            handle_single(state, data).await;
            return;
        }
    }
    handle_single(state, value).await;
}

async fn handle_single(state: &Arc<Mutex<MarketState>>, value: &Value) {
    let Some(event_type) = value.get("event_type").and_then(|v| v.as_str()) else {
        return;
    };

    match event_type {
        "best_bid_ask" => {
            if let Some((asset_id, mid, ask)) = parse_best_bid_ask(value) {
                let mut g = state.lock().await;
                if g.asset_to_outcome.contains_key(&asset_id) {
                    g.mid_prices.insert(asset_id.clone(), mid);
                    g.best_asks.insert(asset_id, ask);
                    log_prices(&g);
                }
            }
        }
        "last_trade_price" => {
            if let Some((asset_id, price)) = parse_last_trade(value) {
                let mut g = state.lock().await;
                if g.asset_to_outcome.contains_key(&asset_id) {
                    g.mid_prices.insert(asset_id, price);
                    log_prices(&g);
                }
            }
        }
        "price_change" => {
            let root = value.get("asset_id").and_then(|v| v.as_str());
            let changes = value
                .get("price_changes")
                .or_else(|| value.get("changes"))
                .and_then(|v| v.as_array());
            if let Some(changes) = changes {
                let mut g = state.lock().await;
                for c in changes {
                    if let Some((id, price)) = parse_price_change(c, root) {
                        if g.asset_to_outcome.contains_key(&id) {
                            g.mid_prices.insert(id, price);
                        }
                    }
                }
                log_prices(&g);
            }
        }
        _ => {}
    }
}

fn log_prices(state: &MarketState) {
    let mut up: Option<f64> = None;
    let mut down: Option<f64> = None;
    for (id, outcome) in &state.asset_to_outcome {
        let price = state.mid_prices.get(id).copied();
        if outcome.eq_ignore_ascii_case("up") {
            up = price;
        } else if outcome.eq_ignore_ascii_case("down") {
            down = price;
        }
    }
    if let (Some(u), Some(d)) = (up, down) {
        eprint!(
            "\rUP: {:.2}c | DOWN: {:.2}c | Sum: {:.2}c   ",
            u * 100.0,
            d * 100.0,
            (u + d) * 100.0
        );
    }
}

fn parse_best_bid_ask(v: &Value) -> Option<(String, f64, f64)> {
    let id = v.get("asset_id")?.as_str()?.to_string();
    let bid = v
        .get("best_bid")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let ask = v
        .get("best_ask")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let mid = match (bid, ask) {
        (Some(b), Some(a)) => (b + a) / 2.0,
        (Some(b), None) => b,
        (None, Some(a)) => a,
        (None, None) => return None,
    };
    Some((id, mid, ask.unwrap_or(mid)))
}

fn parse_last_trade(v: &Value) -> Option<(String, f64)> {
    let id = v.get("asset_id")?.as_str()?.to_string();
    let p = v
        .get("price")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())?;
    Some((id, p))
}

fn parse_price_change(c: &Value, root: Option<&str>) -> Option<(String, f64)> {
    let id = c
        .get("asset_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| root.map(|s| s.to_string()))?;
    let bid = c
        .get("best_bid")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let ask = c
        .get("best_ask")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let fallback = c
        .get("price")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let price = match (bid, ask) {
        (Some(b), Some(a)) => (b + a) / 2.0,
        (Some(b), None) => b,
        (None, Some(a)) => a,
        (None, None) => fallback?,
    };
    Some((id, price))
}

// ── Fee rate & order book ────────────────────────────────────────────────────

pub async fn get_fee_rate(client: &Client, token_id: &str) -> Result<u64> {
    let url = format!("{CLOB_API}/markets/fee-rate?token_id={token_id}");
    let raw = client.get(&url).send().await?.text().await?;
    let v: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse fee rate: {raw}"))?;
    v.get("base_fee")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("missing base_fee: {raw}"))
}

pub async fn get_order_book(client: &Client, token_id: &str) -> Result<OrderBook> {
    let url = format!("{CLOB_API}/book?token_id={token_id}");
    let book: OrderBook = client.get(&url).send().await?.json().await?;
    Ok(book)
}

pub fn calculate_total_ask_size(levels: &[OrderBookLevel]) -> f64 {
    levels
        .iter()
        .take(5)
        .filter_map(|l| l.size.parse::<f64>().ok())
        .sum()
}

// ── L1 EIP-712 Auth (wallet signature for API credential derivation) ────────

async fn l1_auth_signature(
    wallet: &LocalWallet,
    address: Address,
    timestamp: i64,
    nonce: u64,
) -> Result<String> {
    use ethers::types::transaction::eip712::TypedData;

    let td: TypedData = serde_json::from_value(json!({
        "primaryType": "ClobAuth",
        "domain": { "name": "ClobAuthDomain", "version": "1", "chainId": CHAIN_ID },
        "types": {
            "EIP712Domain": [
                {"name": "name",    "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"}
            ],
            "ClobAuth": [
                {"name": "address",   "type": "address"},
                {"name": "timestamp", "type": "string"},
                {"name": "nonce",     "type": "uint256"},
                {"name": "message",   "type": "string"}
            ]
        },
        "message": {
            "address":   format!("{:#x}", address),
            "timestamp": timestamp.to_string(),
            "nonce":     nonce,
            "message":   "This message attests that I control the given wallet"
        }
    }))?;

    let sig = wallet
        .sign_typed_data(&td)
        .await
        .map_err(|e| anyhow!("L1 ClobAuth sign failed: {e}"))?;
    Ok(format!("0x{}", hex::encode(sig.to_vec())))
}

// ── L2 HMAC-SHA256 Auth (for authenticated CLOB API calls) ──────────────────

pub fn l2_signature(
    secret: &str,
    timestamp: i64,
    method: &str,
    path: &str,
    body: &str,
) -> Result<String> {
    let msg = format!("{}{}{}{}", timestamp, method.to_uppercase(), path, body);
    let secret_bytes = BASE64.decode(secret).context("decode L2 secret")?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&secret_bytes).map_err(|e| anyhow!("HMAC error: {e}"))?;
    mac.update(msg.as_bytes());
    Ok(BASE64.encode(mac.finalize().into_bytes()))
}

// ── API credential derivation ────────────────────────────────────────────────

async fn get_or_create_api_creds(
    wallet: &LocalWallet,
    address: Address,
    client: &Client,
) -> Result<ApiCredentials> {
    let ts = now_ms() / 1000;
    let sig = l1_auth_signature(wallet, address, ts, 0).await?;
    let addr = format!("{:#x}", address);

    // Try derive first
    let resp = client
        .get(format!("{CLOB_API}/auth/derive-api-key"))
        .header("POLY_ADDRESS", &addr)
        .header("POLY_SIGNATURE", &sig)
        .header("POLY_TIMESTAMP", ts.to_string())
        .header("POLY_NONCE", "0")
        .send()
        .await?;
    let raw = resp.text().await?;
    let v: Value = serde_json::from_str(&raw)?;

    if let (Some(k), Some(s), Some(p)) = (
        v.get("apiKey").and_then(|v| v.as_str()),
        v.get("secret").and_then(|v| v.as_str()),
        v.get("passphrase").and_then(|v| v.as_str()),
    ) {
        return Ok(ApiCredentials {
            api_key: k.into(),
            secret: s.into(),
            passphrase: p.into(),
        });
    }

    // Fall back to create
    let ts2 = now_ms() / 1000;
    let sig2 = l1_auth_signature(wallet, address, ts2, 0).await?;

    let resp2 = client
        .post(format!("{CLOB_API}/auth/api-key"))
        .header("POLY_ADDRESS", &addr)
        .header("POLY_SIGNATURE", &sig2)
        .header("POLY_TIMESTAMP", ts2.to_string())
        .header("POLY_NONCE", "0")
        .send()
        .await?;
    let raw2 = resp2.text().await?;
    let v2: Value = serde_json::from_str(&raw2)?;

    let api_key = v2.get("apiKey").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow!(
            "Could not get API creds. Server: {raw2}\n\
             If \"Could not create api key\", visit polymarket.com and accept ToS with wallet {addr}."
        )
    })?;
    let secret = v2
        .get("secret")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no secret: {raw2}"))?;
    let passphrase = v2
        .get("passphrase")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no passphrase: {raw2}"))?;

    Ok(ApiCredentials {
        api_key: api_key.into(),
        secret: secret.into(),
        passphrase: passphrase.into(),
    })
}

// ── EIP-712 Order signing ────────────────────────────────────────────────────

pub async fn eip712_order_signature(
    wallet: &LocalWallet,
    address: Address,
    token_id: &str,
    maker_amount: u128,
    taker_amount: u128,
    side: u8,
    salt: u64,
    fee_bps: u64,
    expiration: u64,
) -> Result<String> {
    use ethers::types::transaction::eip712::TypedData;

    let td: TypedData = serde_json::from_value(json!({
        "primaryType": "Order",
        "domain": {
            "name": "Polymarket CTF Exchange", "version": "1",
            "chainId": CHAIN_ID, "verifyingContract": CTF_EXCHANGE_ADDRESS
        },
        "types": {
            "EIP712Domain": [
                {"name": "name",              "type": "string"},
                {"name": "version",           "type": "string"},
                {"name": "chainId",           "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "Order": [
                {"name": "salt",          "type": "uint256"},
                {"name": "maker",         "type": "address"},
                {"name": "signer",        "type": "address"},
                {"name": "taker",         "type": "address"},
                {"name": "tokenId",       "type": "uint256"},
                {"name": "makerAmount",   "type": "uint256"},
                {"name": "takerAmount",   "type": "uint256"},
                {"name": "expiration",    "type": "uint256"},
                {"name": "nonce",         "type": "uint256"},
                {"name": "feeRateBps",    "type": "uint256"},
                {"name": "side",          "type": "uint8"},
                {"name": "signatureType", "type": "uint8"}
            ]
        },
        "message": {
            "salt": salt.to_string(), "maker": format!("{:#x}", address),
            "signer": format!("{:#x}", address), "taker": ZERO_ADDRESS,
            "tokenId": token_id, "makerAmount": maker_amount.to_string(),
            "takerAmount": taker_amount.to_string(), "expiration": expiration.to_string(),
            "nonce": "0", "feeRateBps": fee_bps.to_string(),
            "side": side, "signatureType": 0u8
        }
    }))?;

    let sig = wallet
        .sign_typed_data(&td)
        .await
        .map_err(|e| anyhow!("Order EIP-712 sign failed: {e}"))?;
    Ok(format!("0x{}", hex::encode(sig.to_vec())))
}

// ── Build signed order ───────────────────────────────────────────────────────

pub async fn build_order_request(
    wallet: &Arc<TradingWallet>,
    token_id: &str,
    size: u64,
    price: f64,
    side: &str,
    fee_bps: u64,
    salt: u64,
    order_type: &str,
    expiration: u64,
) -> Result<CreateOrderRequest> {
    let side_uint: u8 = if side == "BUY" { 0 } else { 1 };

    let (maker_amount, taker_amount) = if side_uint == 0 {
        let maker = (price * size as f64 * 1_000_000.0).round() as u128;
        let taker = size as u128 * 1_000_000;
        (maker, taker)
    } else {
        let maker = size as u128 * 1_000_000;
        let taker = (price * size as f64 * 1_000_000.0).round() as u128;
        (maker, taker)
    };

    const MIN_MAKER: u128 = 1_000_000;
    if maker_amount < MIN_MAKER {
        return Err(anyhow!(
            "Maker amount ${:.4} below $1.00 minimum",
            maker_amount as f64 / 1_000_000.0
        ));
    }

    let signature = eip712_order_signature(
        &wallet.wallet,
        wallet.address,
        token_id,
        maker_amount,
        taker_amount,
        side_uint,
        salt,
        fee_bps,
        expiration,
    )
    .await?;

    Ok(CreateOrderRequest {
        order: PolymarketOrderStruct {
            salt,
            maker: format!("{:#x}", wallet.address),
            signer: format!("{:#x}", wallet.address),
            taker: ZERO_ADDRESS.to_string(),
            token_id: token_id.to_string(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            side: side.to_string(),
            expiration: expiration.to_string(),
            nonce: "0".to_string(),
            fee_rate_bps: fee_bps.to_string(),
            signature,
            signature_type: 0,
        },
        owner: wallet.creds.api_key.clone(),
        order_type: order_type.to_string(),
        defer_exec: false,
    })
}

// ── Submit / cancel / poll orders ────────────────────────────────────────────

pub async fn place_single_order(
    client: &Client,
    wallet: &Arc<TradingWallet>,
    order: CreateOrderRequest,
) -> Result<BatchOrderResult> {
    let body = serde_json::to_string(&order)?;
    let ts = now_ms() / 1000;
    let sig = l2_signature(&wallet.creds.secret, ts, "POST", "/order", &body)?;

    let resp = client
        .post(format!("{CLOB_API}/order"))
        .header("Content-Type", "application/json")
        .header("POLY_ADDRESS", format!("{:#x}", wallet.address))
        .header("POLY_SIGNATURE", sig)
        .header("POLY_TIMESTAMP", ts.to_string())
        .header("POLY_API_KEY", &wallet.creds.api_key)
        .header("POLY_PASSPHRASE", &wallet.creds.passphrase)
        .body(body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let err = resp.text().await.unwrap_or_default();
        return Err(anyhow!("order failed: {err}"));
    }
    Ok(resp.json().await?)
}

#[allow(dead_code)]
pub async fn cancel_order(
    client: &Client,
    wallet: &Arc<TradingWallet>,
    order_id: &str,
) -> Result<()> {
    let ts = now_ms() / 1000;
    let body = json!({"orderID": order_id}).to_string();
    let sig = l2_signature(&wallet.creds.secret, ts, "DELETE", "/order", &body)?;

    let resp = client
        .delete(format!("{CLOB_API}/order"))
        .header("Content-Type", "application/json")
        .header("POLY_ADDRESS", format!("{:#x}", wallet.address))
        .header("POLY_SIGNATURE", sig)
        .header("POLY_TIMESTAMP", ts.to_string())
        .header("POLY_API_KEY", &wallet.creds.api_key)
        .header("POLY_PASSPHRASE", &wallet.creds.passphrase)
        .body(body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let err = resp.text().await.unwrap_or_default();
        return Err(anyhow!("cancel failed: {err}"));
    }
    Ok(())
}

pub async fn get_order_status(
    client: &Client,
    wallet: &Arc<TradingWallet>,
    order_id: &str,
) -> Result<OpenOrder> {
    let path = format!("/data/order/{}", order_id);
    let ts = now_ms() / 1000;
    let sig = l2_signature(&wallet.creds.secret, ts, "GET", &path, "")?;

    let resp = client
        .get(format!("{CLOB_API}{path}"))
        .header("POLY_ADDRESS", format!("{:#x}", wallet.address))
        .header("POLY_SIGNATURE", sig)
        .header("POLY_TIMESTAMP", ts.to_string())
        .header("POLY_API_KEY", &wallet.creds.api_key)
        .header("POLY_PASSPHRASE", &wallet.creds.passphrase)
        .send()
        .await?;

    if !resp.status().is_success() {
        let err = resp.text().await.unwrap_or_default();
        return Err(anyhow!("get order status failed: {err}"));
    }
    Ok(resp.json().await?)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
