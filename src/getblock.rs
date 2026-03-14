// GetBlock.io WebSocket — streams live BTC/USD price via Chainlink on Ethereum.
//
// Connects to wss://go.getblock.io/<key> (Ethereum mainnet node).
// Subscribes to `newHeads`, then on each block queries Chainlink BTC/USD
// aggregator's `latestAnswer()` via `eth_call`.  Gives ~12s price updates.

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::env;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

pub(crate) const GETBLOCK_API_KEY_ENV: &str = "GETBLOCK_API_KEY";

// Chainlink BTC/USD Price Feed on Ethereum mainnet (8 decimals).
const CHAINLINK_BTC_USD: &str = "0xF4030086522a5bEEa4988F8cA5B36dbC97BeE88c";
// latestAnswer() selector
const LATEST_ANSWER_SELECTOR: &str = "0x50d25bcd";

#[derive(Debug)]
pub struct BtcPriceState {
    /// BTC price at start of current 5-min window.
    pub window_open_price: Option<f64>,
    /// Most recent BTC price.
    pub latest_price: f64,
    /// Timestamp (ms) of latest update.
    pub latest_ts: i64,
}

impl BtcPriceState {
    pub fn new_shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            window_open_price: None,
            latest_price: 0.0,
            latest_ts: 0,
        }))
    }

    /// Percent change from window open to now.  Positive = BTC going up.
    #[allow(dead_code)]
    pub fn pct_change(&self) -> Option<f64> {
        let open = self.window_open_price?;
        if open <= 0.0 {
            return None;
        }
        Some((self.latest_price - open) / open * 100.0)
    }

    pub fn reset_window(&mut self) {
        self.window_open_price = None;
    }
}

/// Runs forever, reconnecting on error.
pub async fn run_price_stream(state: Arc<Mutex<BtcPriceState>>) {
    loop {
        if let Err(e) = stream_loop(Arc::clone(&state)).await {
            eprintln!("GetBlock WS error: {e:#}. Reconnecting in 3s…");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

async fn stream_loop(state: Arc<Mutex<BtcPriceState>>) -> Result<()> {
    let api_key = env::var(GETBLOCK_API_KEY_ENV)
        .with_context(|| format!("Missing '{GETBLOCK_API_KEY_ENV}' in .env"))?;
    let ws_url = format!("wss://go.getblock.io/{}", api_key.trim());

    let (ws, _) = connect_async(&ws_url).await.context("connect GetBlock WS")?;
    let (mut write, mut read) = ws.split();
    eprintln!("GetBlock WS connected");

    // Subscribe to new block headers
    let sub_msg = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_subscribe",
        "params": ["newHeads"]
    });
    write
        .send(Message::Text(sub_msg.to_string()))
        .await
        .context("send eth_subscribe")?;

    // Query price immediately on connect
    write
        .send(Message::Text(build_price_call(2).to_string()))
        .await?;

    let mut next_id: u64 = 3;

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };

                // JSON-RPC response to our eth_call → parse Chainlink answer
                if v.get("id").is_some() {
                    if let Some(price) = parse_chainlink_answer(&v) {
                        update_price(&state, price).await;
                    }
                }

                // Subscription notification (new block) → query price
                if v.get("method").and_then(|m| m.as_str()) == Some("eth_subscription") {
                    let call = build_price_call(next_id);
                    next_id += 1;
                    write
                        .send(Message::Text(call.to_string()))
                        .await
                        .ok();
                }
            }
            Ok(Message::Ping(data)) => {
                write.send(Message::Pong(data)).await.ok();
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => return Err(anyhow!("GetBlock WS read error: {e}")),
        }
    }

    Ok(())
}

fn build_price_call(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "eth_call",
        "params": [
            { "to": CHAINLINK_BTC_USD, "data": LATEST_ANSWER_SELECTOR },
            "latest"
        ]
    })
}

fn parse_chainlink_answer(response: &Value) -> Option<f64> {
    let hex = response.get("result")?.as_str()?.trim_start_matches("0x");
    let trimmed = hex.trim_start_matches('0');
    if trimmed.is_empty() {
        return None;
    }
    // BTC/USD price fits comfortably in u64 even at 8 decimals.
    let raw = u64::from_str_radix(trimmed, 16).ok()?;
    Some(raw as f64 / 1e8)
}

async fn update_price(state: &Arc<Mutex<BtcPriceState>>, price: f64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut s = state.lock().await;
    s.latest_price = price;
    s.latest_ts = now;

    if s.window_open_price.is_none() {
        s.window_open_price = Some(price);
        eprintln!("BTC window open: ${:.2}", price);
    }
    eprintln!("BTC: ${:.2}", price);
}
