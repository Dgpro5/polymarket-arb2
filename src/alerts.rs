// Discord webhook alerts.
//
// Main webhook   → startup message + successful bets.
// Error webhook  → failed Polymarket transactions (full log).

use reqwest::Client;
use serde_json::json;

use crate::consts::{DISCORD_WEBHOOK_URL, ERROR_DISCORD_WEBHOOK_URL};

// ── Public API ──────────────────────────────────────────────────────────────

/// Send a startup notification to the main Discord channel.
pub async fn send_startup(client: &Client, wallet_address: &str, balance_usdc: f64) {
    let msg = format!(
        "**Bot Started**\nWallet: `{}`\nBalance: **${:.4}** USDC\nStrategy: BET window T-45s → T-15s | Budget: $2/window",
        wallet_address, balance_usdc
    );
    send_main(client, &msg).await;
}

/// Send a successful bet notification to the main Discord channel.
pub async fn send_bet_success(
    client: &Client,
    direction: &str,
    price_cents: f64,
    size_usd: f64,
    order_id: &str,
    btc_pct_change: f64,
    window_slug: &str,
) {
    let msg = format!(
        "**BET PLACED**\nWindow: `{}`\nDirection: **{}**\nPrice: **{:.2}c**\nSize: **${:.2}**\nBTC move: **{:.4}%**\nOrder ID: `{}`",
        window_slug, direction, price_cents, size_usd, btc_pct_change, order_id
    );
    send_main(client, &msg).await;
}

/// Send a failed transaction alert to the error Discord channel.
/// Includes the full error log for debugging.
pub async fn send_tx_error(client: &Client, context: &str, error: &str, details: &str) {
    let msg = format!(
        "**TX FAILED**\nContext: `{}`\nError:\n```\n{}\n```\nDetails:\n```\n{}\n```",
        context, error, details
    );
    send_error(client, &msg).await;
}

// ── Internal ────────────────────────────────────────────────────────────────

async fn send_main(client: &Client, content: &str) {
    send_webhook(client, DISCORD_WEBHOOK_URL, content).await;
}

async fn send_error(client: &Client, content: &str) {
    send_webhook(client, ERROR_DISCORD_WEBHOOK_URL, content).await;
}

async fn send_webhook(client: &Client, url: &str, content: &str) {
    // Discord content limit is 2000 chars; truncate if needed
    let truncated = if content.len() > 1950 {
        format!("{}…\n(truncated)", &content[..1950])
    } else {
        content.to_string()
    };

    let body = json!({ "content": truncated });

    match client.post(url).json(&body).send().await {
        Ok(resp) if !resp.status().is_success() => {
            eprintln!("Discord webhook error: HTTP {}", resp.status());
        }
        Err(e) => {
            eprintln!("Discord webhook send failed: {e:#}");
        }
        _ => {}
    }
}
