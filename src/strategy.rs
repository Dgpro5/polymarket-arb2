// Strategy — bet timing logic for BTCUSD 5-min UP/DOWN markets.
//
// Evaluates whether to place a bet in the final 45–15 seconds of each window,
// based on BTC price change from window open and dynamic confidence thresholds.

use anyhow::Result;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::alerts;
use crate::consts::*;
use crate::binance::BtcPriceState;
use crate::polymarket::{
    MarketState, TradingWallet, build_order_request, calculate_total_ask_size,
    get_order_book, now_ms, place_single_order,
};

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct BetDecision {
    pub direction: String,   // "UP" or "DOWN"
    pub token_id: String,
    pub size_usd: f64,
    pub max_price: f64,
    pub confidence_pct: f64,
    pub btc_pct_change: f64,
}

// ── Core evaluation ─────────────────────────────────────────────────────────

/// Called every second during the last 45s of a window.
/// Returns `Some(BetDecision)` if conditions are met, `None` otherwise.
pub async fn evaluate_bet(
    btc_state: &Arc<Mutex<BtcPriceState>>,
    market_state: &Arc<Mutex<MarketState>>,
    secs_remaining: u64,
    client: &Client,
) -> Option<BetDecision> {
    // Guard: only evaluate within our betting window
    if secs_remaining > BET_WINDOW_START_SECS || secs_remaining < BET_WINDOW_END_SECS {
        return None;
    }

    // Read BTC price state
    let (pct_change, latest_ts) = {
        let btc = btc_state.lock().await;
        let pct = btc.pct_change()?;
        (pct, btc.latest_ts)
    };

    // Guard: stale price data
    let now = now_ms();
    if now - latest_ts > MAX_PRICE_STALENESS_MS {
        eprintln!(
            "  T-{}s | SKIP: stale price data ({}ms old)",
            secs_remaining,
            now - latest_ts
        );
        return None;
    }

    let abs_pct = pct_change.abs();

    // Guard: flat market
    if abs_pct < FLAT_CUTOFF_PCT {
        return None; // silent skip — flat markets are the common case
    }

    // Find matching tier
    let (min_pct, alloc_frac) = match find_tier(secs_remaining) {
        Some((thresh, frac)) => (thresh, frac),
        None => return None,
    };

    if abs_pct < min_pct {
        eprintln!(
            "  T-{}s | BTC: {:.4}% | need {:.4}% | SKIP",
            secs_remaining, pct_change, min_pct
        );
        return None;
    }

    // Direction: positive pct_change → BTC going UP
    let direction = if pct_change > 0.0 { "UP" } else { "DOWN" };

    // Find the token_id for this direction
    let ms = market_state.lock().await;
    let token_id = ms
        .asset_to_outcome
        .iter()
        .find(|(_, outcome)| outcome.eq_ignore_ascii_case(direction))
        .map(|(id, _)| id.clone())?;

    // Check if polymarket already priced in the move
    if let Some(&open_mid) = ms.open_mid_prices.get(&token_id) {
        let current_mid = ms.mid_prices.get(&token_id).copied().unwrap_or(open_mid);
        let drift = current_mid - open_mid;
        if drift > MAX_PM_DRIFT {
            eprintln!(
                "  T-{}s | BTC: {:.4}% {} | PM already moved: {:.2}c → {:.2}c (+{:.2}c) | SKIP",
                secs_remaining, pct_change, direction,
                open_mid * 100.0, current_mid * 100.0, drift * 100.0
            );
            return None;
        }
    }

    // Check ask price
    let ask_price = ms.best_asks.get(&token_id).copied().unwrap_or(1.0);
    if ask_price > MAX_BUY_PRICE {
        eprintln!(
            "  T-{}s | BTC: {:.4}% {} | ask {:.2}c > max {:.0}c | SKIP",
            secs_remaining,
            pct_change,
            direction,
            ask_price * 100.0,
            MAX_BUY_PRICE * 100.0
        );
        return None;
    }

    // Estimate confidence (simplified normal CDF approximation)
    let sigma_remaining = 0.10 / (300_f64).sqrt() * (secs_remaining as f64).sqrt();
    let z = abs_pct / sigma_remaining;
    let confidence = approx_normal_cdf(z);

    // Check edge after fees
    let fee_pct = ms.fee_bps as f64 / 10_000.0;
    let net_edge = confidence - ask_price - fee_pct;
    if net_edge < MIN_EDGE_PCT {
        eprintln!(
            "  T-{}s | BTC: {:.4}% {} | conf {:.1}% | ask {:.2}c | fee {:.1}% | edge {:.1}% < {:.0}% | SKIP",
            secs_remaining, pct_change, direction,
            confidence * 100.0, ask_price * 100.0, fee_pct * 100.0,
            net_edge * 100.0, MIN_EDGE_PCT * 100.0
        );
        return None;
    }

    drop(ms);

    // Check order book depth
    match get_order_book(client, &token_id).await {
        Ok(book) => {
            let depth = calculate_total_ask_size(&book.asks);
            if depth < MIN_ASK_DEPTH_USD {
                eprintln!(
                    "  T-{}s | BTC: {:.4}% {} | depth ${:.0} < ${:.0} | SKIP",
                    secs_remaining, pct_change, direction, depth, MIN_ASK_DEPTH_USD
                );
                return None;
            }
        }
        Err(e) => {
            eprintln!("  T-{}s | order book fetch failed: {e:#} | SKIP", secs_remaining);
            return None;
        }
    }

    let size_usd = PER_WINDOW_MAX_USD * alloc_frac;

    eprintln!(
        "  T-{}s | BTC: {:.4}% {} | conf {:.1}% | ask {:.2}c | edge {:.1}% | ${:.2} → BET",
        secs_remaining,
        pct_change,
        direction,
        confidence * 100.0,
        ask_price * 100.0,
        net_edge * 100.0,
        size_usd
    );

    Some(BetDecision {
        direction: direction.to_string(),
        token_id,
        size_usd,
        max_price: ask_price,
        confidence_pct: confidence * 100.0,
        btc_pct_change: pct_change,
    })
}

// ── Execution ───────────────────────────────────────────────────────────────

/// Place the bet as a FAK (fill-and-kill) order.
/// Returns (success, order_id_or_error).
async fn execute_bet(
    client: &Client,
    wallet: &Arc<TradingWallet>,
    decision: &BetDecision,
    fee_bps: u64,
) -> Result<String> {
    let size = decision.size_usd.floor().max(1.0) as u64; // minimum $1
    let salt = now_ms() as u64;
    let expiration = (now_ms() / 1000 + 300) as u64; // 5 min from now

    let order = build_order_request(
        wallet,
        &decision.token_id,
        size,
        decision.max_price,
        "BUY",
        fee_bps,
        salt,
        "FAK",
        expiration,
    )
    .await?;

    let result = place_single_order(client, wallet, order).await?;

    if result.success {
        eprintln!(
            "  ORDER FILLED: {} {} @ {:.2}c | ${} | id: {}",
            decision.direction,
            result.taking_amount,
            decision.max_price * 100.0,
            size,
            result.order_id
        );
        Ok(result.order_id)
    } else {
        let err_msg = format!(
            "Order rejected: {} | side: {} | price: {:.2}c | size: ${} | error: {}",
            result.error_msg, decision.direction, decision.max_price * 100.0, size, result.status
        );
        Err(anyhow::anyhow!("{}", err_msg))
    }
}

// ── Strategy loop (spawned as a task per window) ────────────────────────────

/// Runs every 1 second during the window. Places at most one bet per window.
pub async fn run_strategy_loop(
    btc_state: Arc<Mutex<BtcPriceState>>,
    market_state: Arc<Mutex<MarketState>>,
    wallet: Arc<TradingWallet>,
    client: Client,
    end_ts: i64,
    window_slug: String,
) {
    let mut bet_placed = false;
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        ticker.tick().await;

        let now = now_ms() / 1000;
        let secs_remaining = (end_ts - now).max(0) as u64;

        // Stop if window is over
        if secs_remaining == 0 {
            break;
        }

        // Only evaluate within our window
        if secs_remaining > BET_WINDOW_START_SECS {
            continue;
        }

        if bet_placed {
            continue;
        }

        if secs_remaining < BET_WINDOW_END_SECS {
            // Past our cutoff, log final BTC state and stop
            let btc = btc_state.lock().await;
            if let Some(pct) = btc.pct_change() {
                eprintln!(
                    "  T-{}s | Window ending | BTC: {:.4}% | no bet placed",
                    secs_remaining, pct
                );
            }
            break;
        }

        if let Some(decision) = evaluate_bet(&btc_state, &market_state, secs_remaining, &client).await {
            let fee_bps = market_state.lock().await.fee_bps;
            match execute_bet(&client, &wallet, &decision, fee_bps).await {
                Ok(order_id) => {
                    // Discord: successful bet
                    alerts::send_bet_success(
                        &client,
                        &decision.direction,
                        decision.max_price * 100.0,
                        decision.size_usd,
                        &order_id,
                        decision.btc_pct_change,
                        &window_slug,
                    )
                    .await;
                    bet_placed = true;
                }
                Err(e) => {
                    let details = format!(
                        "Window: {}\nDirection: {}\nPrice: {:.2}c\nSize: ${:.2}\nBTC change: {:.4}%\nConfidence: {:.1}%",
                        window_slug, decision.direction, decision.max_price * 100.0,
                        decision.size_usd, decision.btc_pct_change, decision.confidence_pct
                    );
                    // Discord: failed tx
                    alerts::send_tx_error(
                        &client,
                        &format!("BET {} on {}", decision.direction, window_slug),
                        &format!("{e:#}"),
                        &details,
                    )
                    .await;
                    eprintln!("  BET ERROR: {e:#}");
                    // Don't retry — risk of double-betting
                    bet_placed = true;
                }
            }
        }
    }

    // Log window close price
    let btc = btc_state.lock().await;
    if let Some(pct) = btc.pct_change() {
        eprintln!(
            "  Window close | BTC: ${:.2} | change: {:.4}% | bet_placed: {}",
            btc.latest_price, pct, bet_placed
        );
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Find the tier for the given seconds remaining.
/// Returns (min_pct_threshold, allocation_fraction) or None.
fn find_tier(secs_remaining: u64) -> Option<(f64, f64)> {
    for &(max_secs, min_pct, alloc) in &TIERS {
        if secs_remaining <= max_secs {
            return Some((min_pct, alloc));
        }
    }
    None
}

/// Fast approximation of the standard normal CDF for z >= 0.
/// Uses Abramowitz & Stegun formula 26.2.17 (max error ~7.5e-8).
fn approx_normal_cdf(z: f64) -> f64 {
    if z < 0.0 {
        return 1.0 - approx_normal_cdf(-z);
    }
    let p = 0.2316419;
    let b1 = 0.319381530;
    let b2 = -0.356563782;
    let b3 = 1.781477937;
    let b4 = -1.821255978;
    let b5 = 1.330274429;

    let t = 1.0 / (1.0 + p * z);
    let t2 = t * t;
    let t3 = t2 * t;
    let t4 = t3 * t;
    let t5 = t4 * t;

    let pdf = (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt();
    1.0 - pdf * (b1 * t + b2 * t2 + b3 * t3 + b4 * t4 + b5 * t5)
}
