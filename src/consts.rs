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
pub const SIMPLESWAP_API_KEY_ENV: &str = "SIMPLESWAP_API_KEY";

// ── POL gas top-up ──────────────────────────────────────────────────────────

pub const POL_LOW_THRESHOLD: f64 = 0.5;
pub const POL_TOP_UP_USDC: f64 = 10.0;

// ── Strategy: bet timing ────────────────────────────────────────────────────

/// Start evaluating bets when this many seconds remain in the window.
pub const BET_WINDOW_START_SECS: u64 = 45;
/// Stop placing bets below this — execution risk too high.
pub const BET_WINDOW_END_SECS: u64 = 15;

/// Below this absolute move, treat market as flat — never bet.
pub const FLAT_CUTOFF_PCT: f64 = 0.03;

/// Tiered thresholds: (max_secs_remaining, min_pct_change, allocation_fraction).
/// Evaluated in order; first match wins.
pub const TIERS: [(u64, f64, f64); 3] = [
    // 25–15s left: high confidence, small move sufficient, bet 80%
    (25, 0.04, 0.80),
    // 36–25s left: medium-high confidence, bet 60%
    (36, 0.06, 0.60),
    // 45–36s left: medium confidence, need larger move, bet 40%
    (45, 0.08, 0.40),
];

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
