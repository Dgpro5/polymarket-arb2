// Binance WebSocket — streams live BTC/USDT price via trade stream.
//
// Connects to wss://stream.binance.com:9443/ws/btcusdt@trade (free, no API key).
// Each trade message gives a real-time price update (sub-second latency).
// Binance sends WebSocket ping frames every 20s; we must respond with pong
// within 60s or the connection is dropped.

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@trade";

/// How often to sample prices for volatility (seconds).
const VOL_SAMPLE_INTERVAL_MS: i64 = 60_000; // 1 minute
/// Maximum number of price samples to keep (1 hour at 1-min intervals).
const VOL_MAX_SAMPLES: usize = 60;
/// Minimum samples needed before we trust realized vol.
const VOL_MIN_SAMPLES: usize = 10;

#[derive(Debug)]
pub struct BtcPriceState {
    /// BTC price at start of current 5-min window.
    pub window_open_price: Option<f64>,
    /// Most recent BTC price.
    pub latest_price: f64,
    /// Timestamp (ms) of latest update.
    pub latest_ts: i64,

    // ── Rolling volatility tracking ──────────────────────────────────────
    /// Recent 1-minute price samples: (timestamp_ms, price).
    price_samples: VecDeque<(i64, f64)>,
    /// Timestamp of last sample taken.
    last_sample_ts: i64,
    /// Cached realized 5-min sigma (updated on each new sample).
    cached_sigma_5min: Option<f64>,
}

impl BtcPriceState {
    pub fn new_shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            window_open_price: None,
            latest_price: 0.0,
            latest_ts: 0,
            price_samples: VecDeque::with_capacity(VOL_MAX_SAMPLES + 1),
            last_sample_ts: 0,
            cached_sigma_5min: None,
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

    /// Returns the realized 5-minute volatility (as a fraction, e.g. 0.002 = 0.2%).
    /// Computed from rolling 1-minute log returns scaled to 5 minutes.
    /// Returns None if insufficient samples (< 10 minutes of data).
    pub fn realized_vol_5min(&self) -> Option<f64> {
        self.cached_sigma_5min
    }

    /// Record a price sample if enough time has elapsed since the last one.
    fn maybe_sample(&mut self, price: f64, now_ms: i64) {
        if now_ms - self.last_sample_ts < VOL_SAMPLE_INTERVAL_MS {
            return;
        }
        self.last_sample_ts = now_ms;
        self.price_samples.push_back((now_ms, price));

        // Evict old samples beyond our window
        while self.price_samples.len() > VOL_MAX_SAMPLES {
            self.price_samples.pop_front();
        }

        // Recompute realized vol
        self.cached_sigma_5min = self.compute_realized_vol();
    }

    /// Compute realized 5-min volatility from 1-minute log returns.
    /// σ_5min = σ_1min × √5
    fn compute_realized_vol(&self) -> Option<f64> {
        if self.price_samples.len() < VOL_MIN_SAMPLES {
            return None;
        }

        // Compute log returns between consecutive samples
        let log_returns: Vec<f64> = self
            .price_samples
            .iter()
            .zip(self.price_samples.iter().skip(1))
            .map(|((_, p1), (_, p2))| (p2 / p1).ln())
            .collect();

        if log_returns.is_empty() {
            return None;
        }

        let n = log_returns.len() as f64;
        let mean = log_returns.iter().sum::<f64>() / n;
        let variance = log_returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
        let sigma_1min = variance.sqrt();

        // Scale 1-min vol to 5-min: σ_5min = σ_1min × √5
        let sigma_5min = sigma_1min * 5.0_f64.sqrt();

        Some(sigma_5min)
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

    // Sample for rolling volatility calculation
    s.maybe_sample(price, now);

    if s.window_open_price.is_none() {
        s.window_open_price = Some(price);
        eprintln!("BTC window open: ${:.2}", price);
    }
}
