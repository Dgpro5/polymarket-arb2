mod alerts;
mod chain;
mod consts;
mod encrypt;
mod binance;
mod polymarket;
mod redemptions;
mod strategy;

use anyhow::{Context, Result};
use reqwest::Client;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use consts::DATA_DIR;

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
    chain::redeem_prior_windows(&client, &private_key).await;

    // ── Preflight: check POL & USDC.e, swap if needed, halt if critical ──
    chain::preflight_balance_check(&client, &wallet).await?;

    // ── Startup alert ───────────────────────────────────────────────────
    alerts::send_startup(&client).await;

    // ── BTC price stream (background) ────────────────────────────────────
    let btc_state = binance::BtcPriceState::new_shared();
    tokio::spawn(binance::run_price_stream(Arc::clone(&btc_state)));

    // ── Background redemption loop ────────────────────────────────────────
    let redeem_client = client.clone();
    let redeem_key = private_key.clone();
    tokio::spawn(redemptions::run_redemption_loop(redeem_client, redeem_key));

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

                // Reset BTC window-open price for the new window.
                // Use Polymarket's Chainlink-sourced "price to beat" as the reference.
                {
                    let mut btc = btc_state.lock().await;
                    btc.reset_window();
                    if let Some(ptb) = market.price_to_beat {
                        btc.window_open_price = Some(ptb);
                        eprintln!("Price to beat (Chainlink): ${:.2}", ptb);
                    } else {
                        eprintln!("WARN: No priceToBeat from Polymarket — will use first Binance price");
                    }
                }

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

                // Spawn strategy evaluation loop (runs every 1s in final 45s)
                let strat_btc = Arc::clone(&btc_state);
                let strat_market = Arc::clone(&state);
                let strat_wallet = Arc::clone(&wallet);
                let strat_client = client.clone();
                let strat_end_ts = market.end_ts;
                let strat_slug = market.slug.clone();
                let strat_condition_id = market.condition_id.clone();
                let strat_handle = tokio::spawn(strategy::run_strategy_loop(
                    strat_btc,
                    strat_market,
                    strat_wallet,
                    strat_client,
                    strat_end_ts,
                    strat_slug,
                    strat_condition_id,
                ));

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
                                strat_handle.abort();
                                continue;
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(remaining + 3)) => {}
                }

                strat_handle.abort();
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
