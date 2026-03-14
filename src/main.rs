mod chain;
mod encrypt;
mod getblock;
mod polymarket;

use anyhow::{Context, Result};
use reqwest::Client;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Constants kept for future use
#[allow(dead_code)]
const DISCORD_WEBHOOK_URL: &str = "https://discord.com/api/webhooks/1473284259363163211/4sgTuuoGlwS4OyJ5x6-QmpPA_Q1gvsIZB9EZrb9zWX6qyA0LMQklz3IupBfINPVnpsMZ";
#[allow(dead_code)]
const ERROR_DISCORD_WEBHOOK_URL: &str = "https://discord.com/api/webhooks/1475092817654055084/_mr0tTCdzyyoJtTBwNqE6KYj6SQ0XEegZFv4j5PejJ0vq2i1Vlt0oi7IFmeAt12j0TQW";

const DATA_DIR: &str = "data";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    fs::create_dir_all(DATA_DIR).context("create data dir")?;

    // ── Private key (encrypted at rest) ──────────────────────────────────
    let private_key = encrypt::get_private_key()?;

    // ── Wallet + API credentials ─────────────────────────────────────────
    let wallet = polymarket::setup_wallet(&private_key).await?;

    let client = Client::new();

    // ── On-chain approvals ───────────────────────────────────────────────
    chain::ensure_approvals(&client, &wallet).await?;
    chain::check_and_top_up_pol(&client, &wallet)
        .await
        .unwrap_or_else(|e| eprintln!("POL top-up check failed: {e:#}"));
    chain::redeem_prior_windows(&client, &private_key).await;

    match chain::get_balance(&client, &wallet.address).await {
        Ok(b) => eprintln!("Balance: ${:.4}", b),
        Err(e) => eprintln!("Could not fetch balance: {e:#}"),
    }

    // ── BTC price stream (background) ────────────────────────────────────
    let btc_state = getblock::BtcPriceState::new_shared();
    tokio::spawn(getblock::run_price_stream(Arc::clone(&btc_state)));

    // ── Fee rate refresh (background) ────────────────────────────────────
    // Will be wired up once a market state exists per window.

    // ── Main loop: cycle through 5-min windows ──────────────────────────
    let mut backoff = 2u64;
    loop {
        match polymarket::discover_active_btc_5m_market(&client).await {
            Ok(market) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let remaining = (market.end_ts - now).max(1) as u64;

                eprintln!("\nWindow: {} | {}s remaining", market.slug, remaining);

                let state = polymarket::MarketState::new_shared(&market);

                // Reset BTC window-open price for the new window
                btc_state.lock().await.reset_window();

                // Start background fee refresh for this window
                let fee_state = Arc::clone(&state);
                let fee_client = client.clone();
                let fee_asset = market.asset_ids.first().cloned();
                let fee_handle = tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        ticker.tick().await;
                        if let Some(ref tid) = fee_asset {
                            if let Ok(fee) = polymarket::get_fee_rate(&fee_client, tid).await {
                                fee_state.lock().await.fee_bps = fee;
                            }
                        }
                    }
                });

                // Run Polymarket WS until window ends or socket closes
                tokio::select! {
                    result = polymarket::run_market_ws(Arc::clone(&state), &market.asset_ids) => {
                        match result {
                            Ok(_) => {}
                            Err(ref e) if e.to_string().contains("NO NEW ASSETS") => {}
                            Err(e) => {
                                eprintln!("\nWS error: {e:#}. Reconnecting in {backoff}s…");
                                tokio::time::sleep(Duration::from_secs(backoff)).await;
                                backoff = (backoff * 2).min(60);
                                fee_handle.abort();
                                continue;
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(remaining + 3)) => {}
                }

                fee_handle.abort();

                // Window closed — housekeeping
                eprintln!("\nWindow closed: {}", market.slug);

                chain::redeem_prior_windows(&client, &private_key).await;
                chain::check_and_top_up_pol(&client, &wallet)
                    .await
                    .unwrap_or_else(|e| eprintln!("POL top-up failed: {e:#}"));

                backoff = 2;
            }
            Err(e) => {
                eprintln!("Market discovery failed: {e:#}. Retrying in {backoff}s…");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(10);
            }
        }
    }
}
