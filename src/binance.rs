// Binance WebSocket — streams live BTC/USDT price via trade stream.
//
// Connects to wss://stream.binance.com:9443/ws/btcusdt@trade (free, no API key).
// Each trade message gives a real-time price update (sub-second latency).
// Binance sends WebSocket ping frames every 20s; we must respond with pong
// within 60s or the connection is dropped.

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@trade";

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

/// Binance trade stream payload.
#[derive(Deserialize)]
struct BinanceTrade {
    /// Trade price as string.
    #[serde(rename = "p")]
    price: String,
    /// Trade time (ms).
    #[serde(rename = "T")]
    trade_time: i64,
}

/// Runs forever, reconnecting on error.
pub async fn run_price_stream(state: Arc<Mutex<BtcPriceState>>) {
    loop {
        if let Err(e) = stream_loop(Arc::clone(&state)).await {
            eprintln!("Binance WS error: {e:#}. Reconnecting in 3s…");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

async fn stream_loop(state: Arc<Mutex<BtcPriceState>>) -> Result<()> {
    let (ws, _) = connect_async(BINANCE_WS_URL)
        .await
        .map_err(|e| anyhow!("connect Binance WS: {e}"))?;
    let (mut write, mut read) = ws.split();
    eprintln!("Binance WS connected (btcusdt@trade)");

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(trade) = serde_json::from_str::<BinanceTrade>(&text) {
                    if let Ok(price) = trade.price.parse::<f64>() {
                        update_price(&state, price, trade.trade_time).await;
                    }
                }
            }
            Ok(Message::Ping(data)) => {
                // Binance sends ping every 20s; must respond within 60s.
                write.send(Message::Pong(data)).await.ok();
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => return Err(anyhow!("Binance WS read error: {e}")),
        }
    }

    Ok(())
}

async fn update_price(state: &Arc<Mutex<BtcPriceState>>, price: f64, trade_time_ms: i64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(trade_time_ms);

    let mut s = state.lock().await;
    s.latest_price = price;
    s.latest_ts = now;

    if s.window_open_price.is_none() {
        s.window_open_price = Some(price);
        eprintln!("BTC window open: ${:.2}", price);
    }
}
