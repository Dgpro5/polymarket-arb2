// Persistent redemption tracker.
//
// After each bet, we record {condition_id, slug, bet_ts} in a JSON ledger file.
// A background task checks every 60s and attempts on-chain redemption once 20
// minutes have elapsed (giving the market time to resolve and settle).
// Failed redemptions are retried every cycle until they succeed.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::alerts;
use crate::chain;
use crate::consts::{PENDING_REDEMPTIONS_FILE, REDEMPTION_DELAY_SECS, REDEMPTION_POLL_INTERVAL_SECS};

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRedemption {
    /// Polymarket conditionId (hex string for on-chain redemption).
    pub condition_id: String,
    /// Human-readable slug (e.g. "btc-updown-5m-1710500400").
    pub slug: String,
    /// Unix timestamp (secs) when the bet was placed.
    pub bet_ts: i64,
    /// Number of redemption attempts so far.
    #[serde(default)]
    pub attempts: u32,
}

// ── Ledger I/O ──────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Load the ledger from disk. Returns empty vec if file doesn't exist.
pub fn load_ledger() -> Vec<PendingRedemption> {
    let path = Path::new(PENDING_REDEMPTIONS_FILE);
    if !path.exists() {
        return Vec::new();
    }
    match fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
            eprintln!("WARN: corrupt redemption ledger, resetting: {e}");
            Vec::new()
        }),
        Err(e) => {
            eprintln!("WARN: cannot read redemption ledger: {e}");
            Vec::new()
        }
    }
}

/// Persist the ledger to disk.
fn save_ledger(entries: &[PendingRedemption]) -> Result<()> {
    let json = serde_json::to_string_pretty(entries)?;
    fs::write(PENDING_REDEMPTIONS_FILE, json).context("write redemption ledger")?;
    Ok(())
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Record a new position that needs future redemption.
pub fn record_pending(condition_id: &str, slug: &str) {
    let mut ledger = load_ledger();

    // Don't duplicate
    if ledger.iter().any(|e| e.condition_id == condition_id) {
        return;
    }

    ledger.push(PendingRedemption {
        condition_id: condition_id.to_string(),
        slug: slug.to_string(),
        bet_ts: now_secs(),
        attempts: 0,
    });

    if let Err(e) = save_ledger(&ledger) {
        eprintln!("WARN: failed to save redemption ledger: {e:#}");
    } else {
        eprintln!("Redemption queued: {} ({})", slug, condition_id);
    }
}

/// Background loop: check the ledger every 60s, redeem entries older than 20min.
/// Runs forever — designed to be spawned as a tokio task.
pub async fn run_redemption_loop(client: Client, private_key: String) {
    let mut ticker = tokio::time::interval(Duration::from_secs(REDEMPTION_POLL_INTERVAL_SECS));

    loop {
        ticker.tick().await;

        let mut ledger = load_ledger();
        if ledger.is_empty() {
            continue;
        }

        let now = now_secs();
        let mut redeemed_ids: Vec<String> = Vec::new();

        for entry in ledger.iter_mut() {
            let age = (now - entry.bet_ts).max(0) as u64;
            if age < REDEMPTION_DELAY_SECS {
                continue; // not yet ripe
            }

            entry.attempts += 1;
            eprintln!(
                "Redeeming {} (attempt #{}, age {}m)…",
                entry.slug,
                entry.attempts,
                age / 60
            );

            match chain::redeem_single(&client, &private_key, &entry.condition_id).await {
                Ok(_) => {
                    eprintln!("Redeemed: {}", entry.slug);
                    redeemed_ids.push(entry.condition_id.clone());
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    // Reverts / insufficient-balance are benign (nothing to redeem)
                    if msg.contains("revert") || msg.contains("insufficient") {
                        eprintln!("Redeem skip (benign): {} — {}", entry.slug, msg);
                        redeemed_ids.push(entry.condition_id.clone());
                    } else {
                        eprintln!(
                            "Redeem FAILED for {} (attempt #{}): {:#}",
                            entry.slug, entry.attempts, e
                        );
                        // Alert on repeated failures
                        if entry.attempts % 5 == 0 {
                            alerts::send_tx_error(
                                &client,
                                &format!("REDEEM {}", entry.slug),
                                &msg,
                                &format!("Attempt #{}, age {}m", entry.attempts, age / 60),
                            )
                            .await;
                        }
                    }
                }
            }
        }

        // Remove successfully redeemed entries
        if !redeemed_ids.is_empty() {
            ledger.retain(|e| !redeemed_ids.contains(&e.condition_id));
            if let Err(e) = save_ledger(&ledger) {
                eprintln!("WARN: failed to update redemption ledger: {e:#}");
            }
        }
    }
}
