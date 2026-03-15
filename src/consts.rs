// Centralized constants for the entire bot.

// ── Discord ─────────────────────────────────────────────────────────────────

pub const DISCORD_WEBHOOK_URL: &str =
    "https://discord.com/api/webhooks/1473284259363164211/4sgTuuoGlwS4OyJ5x6-QmpPA_Q1gvsIZB9EZrb9zWX6qyA0LMQklz3IupBfINPVnpsMZ";
pub const ERROR_DISCORD_WEBHOOK_URL: &str =
    "https://discord.com/api/webhooks/1475092817654055084/_mr0tTCdzyyoJtTBwNqE6KYj6SQ0XEegZFv4j5PejJ0vq2i1Vlt0oi7IFmeAt12j0TQW";

// ── Data storage ────────────────────────────────────────────────────────────

pub const DATA_DIR: &str = "data";

// ── Blockchain / Polygon ────────────────────────────────────────────────────

pub const CHAIN_ID: u64 = 137;
pub const CTF_EXCHANGE_ADDRESS: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
pub const CONDITIONAL_TOKENS_ADDRESS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
pub const USDC_E_POLYGON: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

// ── Polymarket APIs ─────────────────────────────────────────────────────────

pub const GAMMA_API: &str = "https://gamma-api.polymarket.com";
pub const CLOB_API: &str = "https://clob.polymarket.com";
pub const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
pub const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

// ── Environment variable keys ───────────────────────────────────────────────

pub const ANKR_API_KEY_ENV: &str = "ANKR_API_KEY";
// ── POL gas top-up ──────────────────────────────────────────────────────────

pub const POL_LOW_THRESHOLD: f64 = 0.5;
/// If POL balance is at or below this, alert and halt — wallet is critically low.
pub const POL_CRITICAL_THRESHOLD: f64 = 5.0;
/// Fraction of POL to swap into USDC.e when USDC balance is insufficient.
pub const POL_TO_USDC_SWAP_FRACTION: f64 = 0.80;

// ── Strategy: bet timing ────────────────────────────────────────────────────

/// Start evaluating bets when this many seconds remain in the window.
/// Set to 240s (4 min) to catch big early BTC moves before PM reacts.
pub const BET_WINDOW_START_SECS: u64 = 240;
/// Stop placing bets below this — execution risk too high.
pub const BET_WINDOW_END_SECS: u64 = 8;

/// Below this absolute move, treat market as flat — never bet.
/// Lowered for latency arb: we trade the delay between Binance real-time
/// prices and Polymarket's slower orderbook reaction, not the magnitude.
/// BTC 5-min noise floor is ~0.01%, so 0.015% filters jitter.
pub const FLAT_CUTOFF_PCT: f64 = 0.015;

/// BTC annualized volatility estimate (60%). Used to compute the 5-min σ
/// dynamically:  σ_5min = BTC_ANNUAL_VOL / √(525_600 / 5) ≈ 0.185%.
/// This drives the confidence (normal CDF) calculation in strategy.rs.
pub const BTC_ANNUAL_VOL: f64 = 0.60;

/// Late tiers (T-45s to T-8s): percentage-based, small moves suffice.
/// (max_secs_remaining, min_pct_change, allocation_fraction).
/// Evaluated in order; first match wins.
pub const LATE_TIERS: [(u64, f64, f64); 3] = [
    // 25–8s left: close to expiry, even small confirmed moves are predictive
    (25, 0.02, 0.80),
    // 36–25s left
    (36, 0.03, 0.60),
    // 45–36s left
    (45, 0.05, 0.40),
];

/// Early window (T-240s to T-45s): minimum dollar move to even consider betting.
/// Below this the move is noise. The real gate is the confidence factor (≥ 70%).
pub const EARLY_MIN_DOLLAR_MOVE: f64 = 60.0;
/// Minimum confidence (probability BTC stays on the same side of the price to
/// beat) required for early-window bets. The confidence factor uses both the
/// price-to-beat and the current Binance price, scaled by remaining volatility.
pub const EARLY_MIN_CONFIDENCE: f64 = 0.70;
/// Allocation fraction for early-window bets.
pub const EARLY_ALLOC_FRAC: f64 = 0.25;

/// Never pay more than this for an outcome share.
pub const MAX_BUY_PRICE: f64 = 0.92;
/// Maximum polymarket price drift from window open before we consider the move
/// already priced in.  If the outcome we want to bet moved more than this from
/// its opening mid-price, skip — polymarket already reacted.
pub const MAX_PM_DRIFT: f64 = 0.10;
/// Minimum net edge (P_correct - ask - fee) required to bet.
pub const MIN_EDGE_PCT: f64 = 0.03;
/// Minimum total ask depth (USD) in top 5 levels to consider betting.
pub const MIN_ASK_DEPTH_USD: f64 = 50.0;
/// Maximum age (ms) of BTC price data before we consider it stale.
pub const MAX_PRICE_STALENESS_MS: i64 = 20_000;
/// Maximum USDC to risk per 5-minute window.
pub const PER_WINDOW_MAX_USD: f64 = 2.0;

// ── Redemption tracking ────────────────────────────────────────────────────

/// Path to the pending-redemption ledger (inside DATA_DIR).
pub const PENDING_REDEMPTIONS_FILE: &str = "data/pending_redemptions.json";
/// Minimum seconds after bet placement before attempting redemption.
/// Polymarket needs time to resolve the market and settle on-chain.
pub const REDEMPTION_DELAY_SECS: u64 = 20 * 60; // 20 minutes
/// How often (secs) the background redeemer checks the ledger.
pub const REDEMPTION_POLL_INTERVAL_SECS: u64 = 60;
