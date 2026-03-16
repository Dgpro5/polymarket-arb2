// Strategy — bet timing logic for BTCUSD 5-min UP/DOWN markets.
//
// Two independent strategies evaluate each tick; first signal wins.
//
//  STRATEGY 1 — "Impulse" (original):
//    EARLY (T-240s to T-45s): $60 minimum BTC move + tiered confidence (70-80%).
//    LATE  (T-45s to T-8s):   percentage-based thresholds close to expiry.
//    Checks: PM drift, edge.
//
//  STRATEGY 2 — "Momentum" (new):
//    Operates T-240s to T-60s only. $65 minimum BTC move + tiered confidence (65-75%).
//    Checks: edge only (no PM drift — we want to catch big moves regardless).
//    More lenient to ensure we hit at least ~1 trade per window.

use anyhow::Result;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::alerts;
use crate::consts::*;
use crate::binance::BtcPriceState;
use crate::redemptions;
use crate::polymarket::{
    MarketState, TradingWallet, build_order_request, now_ms, place_single_order,
};

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct BetDecision {
    pub strategy: String,    // "Impulse" or "Momentum"
    pub direction: String,   // "UP" or "DOWN"
    pub token_id: String,
    pub size_usd: f64,
    pub max_price: f64,
    pub confidence_pct: f64,
    pub btc_pct_change: f64,
}

// ── Shared helpers ─────────────────────────────────────────────────────────

/// Compute confidence: P(BTC stays on same side of price-to-beat) given remaining vol.
/// Uses realized volatility if available, falls back to BTC_ANNUAL_VOL constant.
fn compute_confidence(abs_pct: f64, secs_remaining: u64, realized_sigma_5min: Option<f64>) -> f64 {
    let sigma_remaining = match realized_sigma_5min {
        Some(s5) => {
            // Scale realized 5-min sigma to remaining time:
            // σ_remaining = σ_5min × √(secs_remaining / 300)
            s5 * (secs_remaining as f64 / 300.0).sqrt()
        }
        None => {
            // Fallback: derive from annualized constant
            let mins_per_year: f64 = 525_600.0;
            BTC_ANNUAL_VOL / mins_per_year.sqrt() * (secs_remaining as f64 / 60.0).sqrt()
        }
    };
    let z = abs_pct / sigma_remaining;
    approx_normal_cdf(z)
}

/// Compute dynamic minimum dollar move threshold from realized volatility.
/// Falls back to the fixed constant if realized vol is unavailable.
fn dynamic_min_dollar_move(btc_price: f64, realized_sigma_5min: Option<f64>, multiplier: f64, fallback: f64) -> f64 {
    match realized_sigma_5min {
        Some(sigma) => {
            let threshold = btc_price * sigma * multiplier;
            // Clamp: never go below $10 or above $500
            threshold.clamp(10.0, 500.0)
        }
        None => fallback,
    }
}

/// Resolve direction + token_id from percent change.
fn resolve_direction(pct_change: f64, ms: &MarketState) -> Option<(String, String, f64)> {
    let direction = if pct_change > 0.0 { "UP" } else { "DOWN" };
    let token_id = ms
        .asset_to_outcome
        .iter()
        .find(|(_, outcome)| outcome.eq_ignore_ascii_case(direction))
        .map(|(id, _)| id.clone())?;
    let ask_price = ms.best_asks.get(&token_id).copied().unwrap_or(1.0);
    Some((direction.to_string(), token_id, ask_price))
}

// ══════════════════════════════════════════════════════════════════════════════
// STRATEGY 1 — "Impulse"
// ══════════════════════════════════════════════════════════════════════════════

/// Strategy 1: Original impulse strategy.
/// EARLY (T-240s to T-45s): $60 floor + tiered confidence (70-80%) + PM drift + edge.
/// LATE  (T-45s to T-8s):   percentage-based tiers + PM drift + edge.
fn evaluate_impulse(
    secs_remaining: u64,
    pct_change: f64,
    abs_pct: f64,
    dollar_move: f64,
    confidence: f64,
    ms: &MarketState,
    min_dollar_move: f64,
) -> Option<BetDecision> {
    // Guard: only within Strategy 1 window
    if secs_remaining > BET_WINDOW_START_SECS || secs_remaining < BET_WINDOW_END_SECS {
        return None;
    }

    // ── Tier gate ───────────────────────────────────────────────────────
    let is_early = secs_remaining > 45;
    let alloc_frac;

    if is_early {
        // EARLY window: dynamic dollar floor + tiered confidence gate
        if dollar_move < min_dollar_move {
            return None;
        }
        match find_early_tier(secs_remaining, confidence) {
            Some(frac) => alloc_frac = frac,
            None => {
                let needed = EARLY_TIERS.iter()
                    .find(|&&(max_s, _, _)| secs_remaining <= max_s)
                    .map(|&(_, min_c, _)| min_c)
                    .unwrap_or(0.80);
                eprintln!(
                    "  [Impulse] T-{}s | BTC: {:.4}% (${:.0}) | conf {:.1}% < {:.0}% | SKIP",
                    secs_remaining, pct_change, dollar_move,
                    confidence * 100.0, needed * 100.0
                );
                return None;
            }
        }
    } else {
        // LATE window: percentage-based tiers
        match find_late_tier(secs_remaining, abs_pct) {
            Some(frac) => alloc_frac = frac,
            None => {
                eprintln!(
                    "  [Impulse] T-{}s | BTC: {:.4}% | need bigger % | SKIP",
                    secs_remaining, pct_change
                );
                return None;
            }
        }
    }

    // Resolve direction
    let (direction, token_id, ask_price) = resolve_direction(pct_change, ms)?;

    // PM drift check (Strategy 1 only)
    if let Some(&open_mid) = ms.open_mid_prices.get(&token_id) {
        let current_mid = ms.mid_prices.get(&token_id).copied().unwrap_or(open_mid);
        let drift = current_mid - open_mid;
        if drift > MAX_PM_DRIFT {
            eprintln!(
                "  [Impulse] T-{}s | BTC: {:.4}% {} | PM drift: {:.2}c → {:.2}c (+{:.2}c) | SKIP",
                secs_remaining, pct_change, direction,
                open_mid * 100.0, current_mid * 100.0, drift * 100.0
            );
            return None;
        }
    }

    // Edge check
    let fee_pct = ms.fee_bps as f64 / 10_000.0;
    let net_edge = confidence - ask_price - fee_pct;
    if net_edge < MIN_EDGE_PCT {
        eprintln!(
            "  [Impulse] T-{}s | BTC: {:.4}% {} | conf {:.1}% | ask {:.2}c | edge {:.1}% < {:.0}% | SKIP",
            secs_remaining, pct_change, direction,
            confidence * 100.0, ask_price * 100.0,
            net_edge * 100.0, MIN_EDGE_PCT * 100.0
        );
        return None;
    }

    let size_usd = PER_WINDOW_MAX_USD * alloc_frac;

    eprintln!(
        "  [Impulse] T-{}s | BTC: {:.4}% (${:.0}) {} | conf {:.1}% | ask {:.2}c | edge {:.1}% | ${:.2} → BET",
        secs_remaining, pct_change, dollar_move, direction,
        confidence * 100.0, ask_price * 100.0, net_edge * 100.0, size_usd
    );

    Some(BetDecision {
        strategy: "Impulse".to_string(),
        direction,
        token_id,
        size_usd,
        max_price: ask_price,
        confidence_pct: confidence * 100.0,
        btc_pct_change: pct_change,
    })
}

// ══════════════════════════════════════════════════════════════════════════════
// STRATEGY 2 — "Momentum"
// ══════════════════════════════════════════════════════════════════════════════

/// Strategy 2: Momentum strategy.
/// Operates T-240s to T-60s. $65 floor + tiered confidence (65-75%) + edge.
/// No PM drift check — we want to catch big moves regardless.
fn evaluate_momentum(
    secs_remaining: u64,
    pct_change: f64,
    _abs_pct: f64,
    dollar_move: f64,
    confidence: f64,
    ms: &MarketState,
    min_dollar_move: f64,
) -> Option<BetDecision> {
    // Guard: only within Strategy 2 window
    if secs_remaining > S2_WINDOW_START_SECS || secs_remaining < S2_WINDOW_END_SECS {
        return None;
    }

    // Dynamic dollar move floor
    if dollar_move < min_dollar_move {
        return None;
    }

    // Tiered confidence gate
    let alloc_frac = match find_s2_tier(secs_remaining, confidence) {
        Some(frac) => frac,
        None => {
            let needed = S2_TIERS.iter()
                .find(|&&(max_s, _, _)| secs_remaining <= max_s)
                .map(|&(_, min_c, _)| min_c)
                .unwrap_or(0.75);
            eprintln!(
                "  [Momentum] T-{}s | BTC: {:.4}% (${:.0}) | conf {:.1}% < {:.0}% | SKIP",
                secs_remaining, pct_change, dollar_move,
                confidence * 100.0, needed * 100.0
            );
            return None;
        }
    };

    // Resolve direction
    let (direction, token_id, ask_price) = resolve_direction(pct_change, ms)?;

    // Edge check (more lenient than Strategy 1)
    let fee_pct = ms.fee_bps as f64 / 10_000.0;
    let net_edge = confidence - ask_price - fee_pct;
    if net_edge < S2_MIN_EDGE_PCT {
        eprintln!(
            "  [Momentum] T-{}s | BTC: {:.4}% {} | conf {:.1}% | ask {:.2}c | edge {:.1}% < {:.0}% | SKIP",
            secs_remaining, pct_change, direction,
            confidence * 100.0, ask_price * 100.0,
            net_edge * 100.0, S2_MIN_EDGE_PCT * 100.0
        );
        return None;
    }

    let size_usd = PER_WINDOW_MAX_USD * alloc_frac;

    eprintln!(
        "  [Momentum] T-{}s | BTC: {:.4}% (${:.0}) {} | conf {:.1}% | ask {:.2}c | edge {:.1}% | ${:.2} → BET",
        secs_remaining, pct_change, dollar_move, direction,
        confidence * 100.0, ask_price * 100.0, net_edge * 100.0, size_usd
    );

    Some(BetDecision {
        strategy: "Momentum".to_string(),
        direction,
        token_id,
        size_usd,
        max_price: ask_price,
        confidence_pct: confidence * 100.0,
        btc_pct_change: pct_change,
    })
}

// ── Core evaluation (runs both strategies) ─────────────────────────────────

/// Called every second during the betting window.
/// Tries Strategy 1 (Impulse) first, then Strategy 2 (Momentum).
/// Returns the first signal that fires.
pub async fn evaluate_bet(
    btc_state: &Arc<Mutex<BtcPriceState>>,
    market_state: &Arc<Mutex<MarketState>>,
    secs_remaining: u64,
    _client: &Client,
) -> Option<BetDecision> {
    // Read BTC price state + realized volatility
    let (pct_change, latest_ts, btc_open, btc_current, realized_vol) = {
        let btc = btc_state.lock().await;
        let pct = btc.pct_change()?;
        let open = btc.window_open_price?;
        (pct, btc.latest_ts, open, btc.latest_price, btc.realized_vol_5min())
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
    let dollar_move = (btc_current - btc_open).abs();

    // Guard: flat market
    if abs_pct < FLAT_CUTOFF_PCT {
        return None;
    }

    // Compute confidence using realized vol (or fallback to constant)
    let confidence = compute_confidence(abs_pct, secs_remaining, realized_vol);

    // Compute dynamic minimum dollar move thresholds
    let s1_min_move = dynamic_min_dollar_move(btc_current, realized_vol, VOL_MOVE_MULTIPLIER, EARLY_MIN_DOLLAR_MOVE);
    let s2_min_move = dynamic_min_dollar_move(btc_current, realized_vol, S2_VOL_MOVE_MULTIPLIER, S2_MIN_DOLLAR_MOVE);

    // Lock market state once for both strategies
    let ms = market_state.lock().await;

    // Try Strategy 1 first
    if let Some(decision) = evaluate_impulse(
        secs_remaining, pct_change, abs_pct, dollar_move, confidence, &ms, s1_min_move,
    ) {
        return Some(decision);
    }

    // Try Strategy 2
    if let Some(decision) = evaluate_momentum(
        secs_remaining, pct_change, abs_pct, dollar_move, confidence, &ms, s2_min_move,
    ) {
        return Some(decision);
    }

    None
}

// ── Execution ───────────────────────────────────────────────────────────────

/// Place the bet as a FAK (fill-and-kill) order.
async fn execute_bet(
    client: &Client,
    wallet: &Arc<TradingWallet>,
    decision: &BetDecision,
    fee_bps: u64,
) -> Result<String> {
    let size = decision.size_usd.floor().max(1.0) as u64;
    let salt = now_ms() as u64;
    let expiration = (now_ms() / 1000 + 300) as u64;

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
            "  [{}] ORDER FILLED: {} {} @ {:.2}c | ${} | id: {}",
            decision.strategy,
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
    condition_id: String,
) {
    let mut bet_placed = false;
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        ticker.tick().await;

        let now = now_ms() / 1000;
        let secs_remaining = (end_ts - now).max(0) as u64;

        if secs_remaining == 0 {
            break;
        }

        if secs_remaining > BET_WINDOW_START_SECS {
            continue;
        }

        if bet_placed {
            continue;
        }

        if secs_remaining < BET_WINDOW_END_SECS {
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

                    redemptions::record_pending(&condition_id, &window_slug);

                    eprintln!(
                        "  [{}] Bet placed successfully for {}",
                        decision.strategy, window_slug
                    );
                    bet_placed = true;
                }
                Err(e) => {
                    let details = format!(
                        "Strategy: {}\nWindow: {}\nDirection: {}\nPrice: {:.2}c\nSize: ${:.2}\nBTC change: {:.4}%\nConfidence: {:.1}%",
                        decision.strategy, window_slug, decision.direction, decision.max_price * 100.0,
                        decision.size_usd, decision.btc_pct_change, decision.confidence_pct
                    );
                    alerts::send_tx_error(
                        &client,
                        &format!("[{}] BET {} on {}", decision.strategy, decision.direction, window_slug),
                        &format!("{e:#}"),
                        &details,
                    )
                    .await;
                    eprintln!("  [{}] BET ERROR: {e:#}", decision.strategy);
                    bet_placed = true;
                }
            }
        }
    }

    let btc = btc_state.lock().await;
    if let Some(pct) = btc.pct_change() {
        eprintln!(
            "  Window close | BTC: ${:.2} | change: {:.4}% | bet_placed: {}",
            btc.latest_price, pct, bet_placed
        );
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Find the matching late tier (T-45s to T-8s, percentage-based).
fn find_late_tier(secs_remaining: u64, abs_pct: f64) -> Option<f64> {
    for &(max_secs, min_pct, alloc) in &LATE_TIERS {
        if secs_remaining <= max_secs {
            return if abs_pct >= min_pct { Some(alloc) } else { None };
        }
    }
    None
}

/// Find the matching early tier for Strategy 1 (T-240s to T-45s, confidence-based).
fn find_early_tier(secs_remaining: u64, confidence: f64) -> Option<f64> {
    for &(max_secs, min_conf, alloc) in &EARLY_TIERS {
        if secs_remaining <= max_secs {
            return if confidence >= min_conf { Some(alloc) } else { None };
        }
    }
    None
}

/// Find the matching tier for Strategy 2 (T-240s to T-60s, confidence-based).
fn find_s2_tier(secs_remaining: u64, confidence: f64) -> Option<f64> {
    for &(max_secs, min_conf, alloc) in &S2_TIERS {
        if secs_remaining <= max_secs {
            return if confidence >= min_conf { Some(alloc) } else { None };
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
