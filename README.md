# polymarket-arb2

Polymarket BTC/USD 5-minute UP/DOWN predictive trading bot on Polygon. Streams real-time BTC price from Binance, monitors Polymarket outcome prices via WebSocket, and places directional bets when a significant BTC move is detected but Polymarket hasn't reacted yet.

## How It Works

1. BTC trades on Binance in real-time (sub-second updates via `btcusdt@trade` WebSocket stream)
2. Each 5-minute window, a new `btc-updown-5m-{timestamp}` market is created on Polymarket with UP and DOWN outcomes
3. The bot tracks BTC's percent change from the window-open price
4. Two strategies evaluate each tick (Impulse + Momentum). If BTC has moved significantly, the first strategy to fire places a directional bet (UP or DOWN)
5. After windows close and resolve, positions are automatically redeemed for USDC

## Project Structure

```
src/
├── main.rs        — Orchestrator: startup, main loop, task spawning
├── binance.rs     — Binance WebSocket: real-time BTC/USDT price feed
├── strategy.rs    — Bet evaluation: tiered thresholds, confidence, edge, execution
├── polymarket.rs  — Polymarket CLOB: market discovery, WS prices, auth, orders
├── chain.rs       — On-chain: approvals, balance, redemption, POL top-up
├── encrypt.rs     — Argon2id + AES-256-GCM private key encryption at rest
├── alerts.rs      — Discord webhook notifications (startup, bets, errors)
└── consts.rs      — All constants and configuration parameters
```

## Startup Flow (`main.rs`)

### `main()`
1. Loads `.env` via `dotenvy::dotenv()`
2. Creates `data/` directory
3. Calls `encrypt::get_private_key()` — prompts for password (or first-time key + password setup)
4. Calls `polymarket::setup_wallet()` — parses private key into `LocalWallet`, derives Polymarket API credentials
5. Calls `chain::ensure_approvals()` — checks/sets USDC allowance and ERC-1155 approval on-chain
6. Calls `chain::redeem_prior_windows()` — redeems positions from last 5 expired windows
7. Calls `chain::preflight_balance_check()` — checks POL & USDC.e balances:
   - If POL < 5 and USDC.e available: swaps $25 USDC.e → ~50 POL via OpenOcean
   - If POL still <= 0.5 after swap attempt: sends Discord alert and **halts the bot**
   - If USDC.e < `PER_WINDOW_MAX_USD` ($2): swaps 80% of POL to USDC.e via OpenOcean
8. Calls `alerts::send_startup()` — sends Discord notification with wallet address and balance
9. Spawns `binance::run_price_stream()` as a background tokio task
10. Enters the main 5-minute window loop

### Main Loop (repeats every 5-minute window)

Each iteration:
1. `polymarket::discover_active_btc_5m_market()` — finds current window's market via Gamma API
2. `btc_state.lock().await.reset_window()` — clears BTC window-open price for fresh tracking
3. Spawns **fee rate refresh task** — polls `polymarket::get_fee_rate()` every 1 second
4. Spawns **strategy evaluation task** — runs `strategy::run_strategy_loop()` every 1 second
5. Runs `polymarket::run_market_ws()` — connects to Polymarket CLOB WebSocket, streams UP/DOWN prices until window ends
6. On window close: calls `chain::redeem_prior_windows()` and `chain::check_and_top_up_pol()`
7. On WS error: exponential backoff reconnect (2s, 4s, 8s, ... up to 60s)
8. On market discovery error: backoff up to 10s

---

## BTC Price Feed (`binance.rs`)

### `BtcPriceState` (shared via `Arc<Mutex<...>>`)

| Field | Type | Description |
|-------|------|-------------|
| `window_open_price` | `Option<f64>` | BTC price at start of current 5-min window (set on first update after `reset_window()`) |
| `latest_price` | `f64` | Most recent BTC trade price |
| `latest_ts` | `i64` | Timestamp (ms) of latest update |

### `BtcPriceState::pct_change()`
Returns `Some(pct)` where `pct = (latest_price - window_open_price) / window_open_price * 100.0`. Positive means BTC going up. Returns `None` if no window open price set yet.

### `BtcPriceState::reset_window()`
Clears `window_open_price` to `None`. Called at the start of each new 5-minute window so the next price update becomes the new open price.

### `run_price_stream(state)`
Runs forever in a background task. On error, logs and reconnects after 3 seconds.

### `stream_loop(state)`
1. Connects to `wss://stream.binance.com:9443/ws/btcusdt@trade`
2. Reads messages in a loop:
   - **Text messages**: Deserializes `BinanceTrade` JSON (`{"p": "price", "T": trade_time_ms, ...}`), parses price as `f64`, calls `update_price()`
   - **Ping frames**: Responds with Pong immediately (Binance sends ping every 20s; must respond within 60s or connection drops)
   - **Close frames**: Breaks loop (triggers reconnect)
   - **Errors**: Returns error (triggers reconnect)

### `BinanceTrade` (deserialized from Binance JSON)

| JSON field | Rust field | Description |
|------------|------------|-------------|
| `"p"` | `price: String` | Trade price |
| `"T"` | `trade_time: i64` | Trade timestamp (ms) |

### `update_price(state, price, trade_time_ms)`
- Sets `latest_price` and `latest_ts`
- If `window_open_price` is `None`, sets it to current price and logs "BTC window open: $XX.XX"

---

## Strategy Evaluation (`strategy.rs`)

The bot runs **two independent strategies** per tick. The first one to fire wins (max 1 bet per window). Every log line and Discord alert includes `[Impulse]` or `[Momentum]` to identify which strategy triggered.

### `BetDecision` (returned when all checks pass)

| Field | Type | Description |
|-------|------|-------------|
| `strategy` | `String` | `"Impulse"` or `"Momentum"` — which strategy triggered |
| `direction` | `String` | `"UP"` or `"DOWN"` |
| `token_id` | `String` | Polymarket CLOB token ID for the chosen outcome |
| `size_usd` | `f64` | USDC amount to bet (based on tier allocation) |
| `max_price` | `f64` | Maximum price willing to pay (current ask price) |
| `confidence_pct` | `f64` | Estimated probability the outcome wins (0-100) |
| `btc_pct_change` | `f64` | BTC percent change from window open |

### Shared Checks (both strategies)

**Price staleness**: If BTC price is older than 20s, skip.
**Flat market filter**: If `|pct_change| < 0.015%`, skip silently.
**Confidence**: `P(BTC stays on same side of price-to-beat)` via normal CDF on remaining volatility.

### Strategy 1 — "Impulse" (T-240s to T-8s)

The original latency-arb strategy. Races Polymarket before its orderbook reprices.

**Early window (T-240s to T-45s)**:
- Requires $60 minimum BTC dollar move
- Tiered confidence gate (earlier = stricter):
  - 120–45s left: 70% confidence, 30% alloc
  - 180–120s left: 75% confidence, 25% alloc
  - 240–180s left: 80% confidence, 20% alloc

**Late window (T-45s to T-8s)**:
- Percentage-based tiers (small confirmed moves suffice):
  - 25–8s left: 0.02% move, 80% alloc
  - 36–25s left: 0.03% move, 60% alloc
  - 45–36s left: 0.05% move, 40% alloc

**Checks**: PM drift (skip if Polymarket already moved >10c), edge ≥ 3%.

### Strategy 2 — "Momentum" (T-240s to T-60s)

Catches big BTC moves even if Polymarket has started repricing. More lenient to ensure ~1 trade per window.

- Requires $65 minimum BTC dollar move
- Tiered confidence gate (more lenient than Impulse):
  - 120–60s left: 65% confidence, 30% alloc
  - 180–120s left: 70% confidence, 25% alloc
  - 240–180s left: 75% confidence, 20% alloc

**Checks**: edge ≥ 2% only. **No PM drift check** — if there's a big price move, we want it regardless.

### `evaluate_bet(btc_state, market_state, secs_remaining, client)`

Called every 1 second. Computes shared state (BTC change, confidence), then tries Strategy 1, then Strategy 2. Returns first signal.

### `execute_bet(client, wallet, decision, fee_bps)`
- Computes `size = max(floor(size_usd), 1)` (minimum $1)
- Sets `salt = now_ms()`, `expiration = now + 300s`
- Calls `build_order_request()` with order type `"FAK"` (fill-and-kill)
- Calls `place_single_order()` to submit
- On success: logs `[Strategy] ORDER FILLED`, returns order ID
- On failure: returns error with rejection details

### `run_strategy_loop(btc_state, market_state, wallet, client, end_ts, window_slug)`
Spawned as a tokio task per window. Ticks every 1 second. Max 1 bet per window.

### `approx_normal_cdf(z)`
Standard normal CDF approximation using Abramowitz & Stegun formula 26.2.17. Handles negative z via symmetry.

---

## Polymarket Integration (`polymarket.rs`)

### Types

**`MarketInfo`** — discovered market metadata:
| Field | Type | Description |
|-------|------|-------------|
| `slug` | `String` | e.g. `"btc-updown-5m-1710432000"` |
| `end_ts` | `i64` | Unix seconds when the window ends |
| `condition_id` | `String` | Hex condition ID for CTF redemption |
| `asset_ids` | `Vec<String>` | CLOB token IDs for each outcome |
| `outcomes` | `Vec<String>` | Outcome names (e.g. `["Up", "Down"]`) |

**`MarketState`** — live price tracking (shared via `Arc<Mutex<...>>`):
| Field | Type | Description |
|-------|------|-------------|
| `market_slug` | `String` | Current window slug |
| `condition_id` | `String` | Condition ID |
| `asset_to_outcome` | `HashMap<String, String>` | Maps token_id → outcome name |
| `best_asks` | `HashMap<String, f64>` | Current best ask per token |
| `mid_prices` | `HashMap<String, f64>` | Current mid-price per token |
| `open_mid_prices` | `HashMap<String, f64>` | Window-open mid-price per token (captured on first WS update) |
| `fee_bps` | `u64` | Current maker fee in basis points |

**`TradingWallet`** — wallet + API credentials:
| Field | Type | Description |
|-------|------|-------------|
| `wallet` | `LocalWallet` | Ethers signer for EIP-712 and tx signing |
| `address` | `Address` | Wallet address |
| `creds` | `ApiCredentials` | `api_key`, `secret`, `passphrase` for CLOB API |

**`PolymarketOrderStruct`** — EIP-712 signed order:
| Field | Description |
|-------|-------------|
| `salt` | Unique ID (timestamp-based) |
| `maker` / `signer` | Wallet address |
| `taker` | `0x0` (anyone can fill) |
| `tokenId` | CLOB asset ID |
| `makerAmount` | USDC to spend (6 decimals) |
| `takerAmount` | Tokens to receive (6 decimals) |
| `side` | `"BUY"` or `"SELL"` |
| `expiration` | Unix seconds (0 = immediate, >0 = GTD) |
| `nonce` | `"0"` |
| `feeRateBps` | Fee in basis points |
| `signature` | EIP-712 hex signature |
| `signatureType` | `0` (EOA) |

### Wallet Setup

#### `setup_wallet(private_key)`
1. Parses private key into `LocalWallet`
2. Calls `get_or_create_api_creds()` to obtain Polymarket CLOB API credentials
3. Returns `Arc<TradingWallet>`

### Market Discovery

#### `compute_current_slug()`
Computes `btc-updown-5m-{start_ts}` where `start_ts = now - (now % 300)`. Returns `(slug, end_ts)`.

#### `discover_active_btc_5m_market(client)`
1. Computes current slug
2. Queries `GET {GAMMA_API}/markets?slug={slug}`
3. Parses `conditionId`, `clobTokenIds`, `outcomes` from response
4. Validates token count matches outcome count
5. Returns `MarketInfo`

#### `parse_market_info(market)` / `parse_string_array(value)`
JSON parsing helpers. Handles both native JSON arrays and stringified JSON arrays in API responses.

### WebSocket Price Stream

#### `run_market_ws(state, asset_ids)`
1. Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/market`
2. Sends subscribe: `{"type": "market", "assets_ids": [...]}`
3. Handles messages:
   - `"ping"` text → responds with `"pong"` text
   - `"pong"` text → ignored
   - `"NO NEW ASSETS"` → returns error (triggers window refresh)
   - JSON → delegates to `handle_clob_message()`
   - WebSocket Ping frame → responds with Pong frame
   - Close frame → breaks
   - Error → returns error

#### `handle_clob_message(state, value)`
Dispatches to `handle_single()`. Handles top-level arrays, nested `data` arrays, and single objects.

#### `handle_single(state, value)`
Processes by `event_type`:
- **`"best_bid_ask"`**: Parses bid/ask, computes mid-price `(bid + ask) / 2`, updates `mid_prices` and `best_asks`. On first update per asset, captures `open_mid_prices` for the pre-move check.
- **`"last_trade_price"`**: Updates `mid_prices` with last trade price.
- **`"price_change"`**: Batch update — iterates `price_changes`/`changes` array, updates `mid_prices`.

#### `log_prices(state)`
Prints `\rUP: XX.XXc | DOWN: YY.YYc | Sum: ZZ.ZZc` to stderr (overwriting in-place).

#### Price Parsing Helpers
- `parse_best_bid_ask(v)` → `(asset_id, mid, ask)` — mid = average of bid/ask, falls back to whichever is available
- `parse_last_trade(v)` → `(asset_id, price)`
- `parse_price_change(c, root)` → `(asset_id, price)` — supports root asset_id inheritance

### Fee Rate & Order Book

#### `get_fee_rate(client, token_id)`
`GET {CLOB_API}/markets/fee-rate?token_id={token_id}` → returns `base_fee` as `u64` basis points.

#### `get_order_book(client, token_id)`
`GET {CLOB_API}/book?token_id={token_id}` → returns `OrderBook { bids, asks }` with `OrderBookLevel { price, size }`.

#### `calculate_total_ask_size(levels)`
Sums the `size` of the top 5 ask levels. Used for depth check.

### Authentication

#### `l1_auth_signature(wallet, address, timestamp, nonce)`
EIP-712 typed-data signature for `ClobAuth` domain. Used to derive or create API credentials.

Domain: `ClobAuthDomain`, version `1`, chainId `137`.
Message: wallet address, timestamp, nonce, attestation text.

#### `l2_signature(secret, timestamp, method, path, body)`
HMAC-SHA256 per-request authentication.
- Message: `{timestamp}{METHOD}{path}{body}`
- Key: base64-decoded API secret
- Result: base64-encoded HMAC

#### `get_or_create_api_creds(wallet, address, client)`
1. Signs L1 ClobAuth message
2. Tries `GET /auth/derive-api-key` (existing credentials)
3. If that fails, tries `POST /auth/api-key` (create new credentials)
4. Returns `ApiCredentials { api_key, secret, passphrase }`

### Order Signing & Building

#### `eip712_order_signature(wallet, address, token_id, maker_amount, taker_amount, side, salt, fee_bps, expiration)`
Signs an EIP-712 `Order` typed data with domain `"Polymarket CTF Exchange"`, version `1`, chainId `137`, verifyingContract `CTF_EXCHANGE_ADDRESS`.

#### `build_order_request(wallet, token_id, size, price, side, fee_bps, salt, order_type, expiration)`
1. Computes `maker_amount` and `taker_amount` based on side:
   - BUY: maker = `price * size * 1e6`, taker = `size * 1e6`
   - SELL: maker = `size * 1e6`, taker = `price * size * 1e6`
2. Validates `maker_amount >= $1.00` (minimum order)
3. Signs via `eip712_order_signature()`
4. Returns `CreateOrderRequest` with `order_type` (e.g. `"FAK"`)

### Order Submission & Management

#### `place_single_order(client, wallet, order)`
`POST {CLOB_API}/order` with HMAC-SHA256 auth headers (`POLY_ADDRESS`, `POLY_SIGNATURE`, `POLY_TIMESTAMP`, `POLY_API_KEY`, `POLY_PASSPHRASE`). Returns `BatchOrderResult { success, order_id, status, error_msg, taking_amount, making_amount }`.

#### `cancel_order(client, wallet, order_id)`
`DELETE {CLOB_API}/order` with `{"orderID": order_id}` body. Authenticated with HMAC.

#### `get_order_status(client, wallet, order_id)`
`GET {CLOB_API}/data/order/{order_id}`. Returns `OpenOrder { id, status, original_size, size_matched }`.

#### `now_ms()`
Current time as Unix milliseconds.

---

## On-Chain Operations (`chain.rs`)

### Approvals

#### `ensure_approvals(client, wallet)`
Calls both `ensure_allowance()` and `ensure_ctf_token_approval()`.

#### `ensure_allowance(client, wallet, spender)`
1. Checks current USDC.e allowance via `get_allowance()` (`allowance(owner, spender)` on USDC.e contract)
2. If >= $1000, returns OK
3. Otherwise sends `approve(spender, type(uint256).max)` transaction:
   - Gets nonce via `eth_getTransactionCount`
   - Gets gas price via `eth_gasPrice`, multiplies by 3
   - Signs legacy tx with ethers, broadcasts via `eth_sendRawTransaction`
   - Waits for receipt via `eth_getTransactionReceipt` (polls every 1s, timeout 30s)

#### `ensure_ctf_token_approval(client, wallet)`
1. Checks `isApprovedForAll(owner, CTF_EXCHANGE)` on ConditionalTokens contract
2. If approved, returns OK
3. Otherwise sends `setApprovalForAll(CTF_EXCHANGE, true)` transaction (same signing flow as allowance)

### Balance

#### `get_balance(client, address)`
Calls `balanceOf(address)` on USDC.e contract (`0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174`) via `eth_call`. Returns balance in USDC (6 decimal conversion).

### Position Redemption

#### `redeem_prior_windows(client, private_key)`
Scans the last 5 resolved 5-minute windows (skips the most recent one):
1. For `i in 2..=6`: computes window timestamp `ws = floor(now/300) * 300 - i * 300`
2. Queries `GET {GAMMA_API}/markets?slug=btc-updown-5m-{ws}` for `conditionId`
3. Calls `redeem_positions(private_key, condition_id)` for each
4. On success: increments counter
5. On error containing "revert" or "insufficient": silently skips (no positions to redeem)
6. On other errors: logs warning
7. Returns count of successfully redeemed windows

#### `redeem_positions(private_key, condition_id_hex)`
1. Creates `LocalSigner` from private key with Polygon chain ID
2. Builds Alloy provider with wallet signer
3. Creates `CtfClient` from `polymarket-client-sdk`
4. Builds `RedeemPositionsRequest::for_binary_market(USDC_E_POLYGON, condition_id)`
5. Sets `index_sets = [1, 2]` (both UP and DOWN positions)
6. Submits on-chain redemption transaction

### Preflight Balance Check

#### `preflight_balance_check(client, wallet)`
Runs on startup before the main loop. Ensures the wallet has enough funds to operate:
1. Fetches POL and USDC.e balances
2. If POL < 5 and USDC.e >= $1: swaps $25 USDC.e → ~50 POL via OpenOcean (top-up first)
3. If POL still <= 0.5 after swap attempt: sends Discord alert and **halts the bot**
4. If USDC.e >= `PER_WINDOW_MAX_USD ($2)`: returns OK (sufficient for trading)
5. Otherwise swaps 80% of POL to USDC.e via OpenOcean

#### `send_pol_transfer(client, wallet, to_address, amount_pol)`
Sends native POL via a legacy transaction (value transfer, no calldata). Gas: 21,000, gas price: 3x current. Waits for on-chain confirmation.

### POL Gas Top-Up

#### `check_and_top_up_pol(client, wallet)`
Called after every window closes. Ensures the wallet has enough POL for gas fees.

1. Fetches POL balance via `eth_getBalance`
2. If POL >= `POL_LOW_THRESHOLD (5.0)` → returns OK (enough gas)
3. If POL < 5 and USDC.e >= $1:
   - Swaps `$25 USDC.e → ~50 POL` via OpenOcean DEX aggregator
   - Caps swap at available USDC.e balance
4. If POL < 5 and USDC.e < $1: logs warning, skips (cannot top up)

### RPC Helpers

- `ankr_rpc()` — Returns `https://rpc.ankr.com/polygon/{ANKR_API_KEY}`
- `get_nonce(client, rpc_url, address)` — `eth_getTransactionCount`
- `get_gas_price(client, rpc_url)` — `eth_gasPrice`
- `send_raw_tx(client, rpc_url, raw_tx)` — `eth_sendRawTransaction`, returns tx hash
- `wait_for_receipt(client, rpc_url, tx_hash)` — Polls `eth_getTransactionReceipt` every 1s for 30s. Checks `status == 0x1`.

---

## Encryption (`encrypt.rs`)

### `get_private_key()`
- If `data/key.enc` exists: prompts for password, decrypts, returns private key
- If not: prompts for private key + password (min 8 chars, confirmed twice), encrypts and stores

### `encrypt_and_store(private_key, password)`
1. Generates 16-byte cryptographic salt and 12-byte nonce via `getrandom`
2. Derives 256-bit key via `Argon2id` (memory-hard, GPU/ASIC resistant)
3. Encrypts private key with `AES-256-GCM` (authenticated encryption)
4. Writes `[16-byte salt][12-byte nonce][ciphertext + 16-byte GCM tag]` to `data/key.enc`

### `decrypt(password)`
1. Reads `data/key.enc`
2. Splits into salt, nonce, ciphertext
3. Derives key via Argon2id with the stored salt
4. Decrypts with AES-256-GCM
5. On wrong password: AES-GCM authentication fails ("wrong password or corrupt file")

### `derive_key(password, salt)`
Argon2id with default parameters → 32-byte key.

---

## Discord Alerts (`alerts.rs`)

### `send_startup(client, wallet_address, balance_usdc)`
Sends to main webhook:
```
Bot Started
Wallet: 0x...
Balance: $X.XXXX USDC
Strategy: BET window T-45s -> T-15s | Budget: $2/window
```

### `send_bet_success(client, direction, price_cents, size_usd, order_id, btc_pct_change, window_slug)`
Sends to main webhook:
```
BET PLACED
Window: btc-updown-5m-XXXX
Direction: UP/DOWN
Price: XX.XXc
Size: $X.XX
BTC move: X.XXXX%
Order ID: ...
```

### `send_low_pol_alert(client, pol_balance)`
Sends to error webhook when POL balance is critically low (<= 5 POL). Bot halts after this alert.

### `send_tx_error(client, context, error, details)`
Sends to error webhook with full error log and trade details in code blocks.

### `send_webhook(client, url, content)`
Posts JSON `{"content": "..."}` to Discord webhook URL. Truncates to 1950 chars if needed (Discord 2000 char limit).

---

## Constants (`consts.rs`)

### Discord
| Constant | Value |
|----------|-------|
| `DISCORD_WEBHOOK_URL` | Main channel (startup + bets) |
| `ERROR_DISCORD_WEBHOOK_URL` | Error channel (failed TXs) |

### Blockchain (Polygon)
| Constant | Value |
|----------|-------|
| `CHAIN_ID` | `137` |
| `CTF_EXCHANGE_ADDRESS` | `0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E` |
| `CONDITIONAL_TOKENS_ADDRESS` | `0x4D97DCd97eC945f40cF65F87097ACe5EA0476045` |
| `USDC_E_POLYGON` | `0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174` |

### Polymarket APIs
| Constant | Value |
|----------|-------|
| `GAMMA_API` | `https://gamma-api.polymarket.com` |
| `CLOB_API` | `https://clob.polymarket.com` |
| `CLOB_WS_URL` | `wss://ws-subscriptions-clob.polymarket.com/ws/market` |

### Environment Variables
| Constant | Env Var | Required |
|----------|---------|----------|
| `ANKR_API_KEY_ENV` | `ANKR_API_KEY` | Yes (Polygon RPC) |
| `SIMPLESWAP_API_KEY_ENV` | `SIMPLESWAP_API_KEY` | No (POL top-up) |

### POL Gas Top-Up & Preflight
| Constant | Value | Description |
|----------|-------|-------------|
| `POL_LOW_THRESHOLD` | `5.0` | Trigger USDC.e→POL top-up when POL drops below this |
| `POL_TOP_UP_TARGET` | `50.0` | Target POL to acquire per swap |
| `POL_TOP_UP_USDC` | `25.0` | USDC.e to spend per top-up (~50 POL) |
| `POL_CRITICAL_THRESHOLD` | `0.5` | Alert + halt if POL at or below this AND no USDC.e |
| `POL_TO_USDC_SWAP_FRACTION` | `0.80` | Fraction of POL to swap to USDC.e when USDC is insufficient |

### Strategy Parameters (shared)
| Constant | Value | Description |
|----------|-------|-------------|
| `BET_WINDOW_START_SECS` | `240` | Start evaluating at T-240s |
| `BET_WINDOW_END_SECS` | `8` | Stop placing bets at T-8s |
| `FLAT_CUTOFF_PCT` | `0.015` | Minimum BTC move % to consider |
| `MAX_PM_DRIFT` | `0.10` | Max PM drift (10c) — Strategy 1 only |
| `MIN_EDGE_PCT` | `0.03` | Minimum net edge (3%) — Strategy 1 |
| `MAX_PRICE_STALENESS_MS` | `20,000` | Reject BTC price older than 20s |
| `PER_WINDOW_MAX_USD` | `2.0` | Max USDC risk per window |

### Strategy 1 (Impulse) Tiers

**Early (T-240s to T-45s)** — $60 min dollar move + confidence:
| Seconds Left | Min Confidence | Budget Allocation |
|-------------|---------------|-------------------|
| 120–45s | 70% | 30% ($0.60) |
| 180–120s | 75% | 25% ($0.50) |
| 240–180s | 80% | 20% ($0.40) |

**Late (T-45s to T-8s)** — percentage-based:
| Seconds Left | Min BTC Move | Budget Allocation |
|-------------|-------------|-------------------|
| 25–8s | 0.02% | 80% ($1.60) |
| 36–25s | 0.03% | 60% ($1.20) |
| 45–36s | 0.05% | 40% ($0.80) |

### Strategy 2 (Momentum) Parameters
| Constant | Value | Description |
|----------|-------|-------------|
| `S2_WINDOW_START_SECS` | `240` | Operates from T-240s |
| `S2_WINDOW_END_SECS` | `60` | Stops at T-60s |
| `S2_MIN_DOLLAR_MOVE` | `$65` | Higher dollar floor |
| `S2_MIN_EDGE_PCT` | `0.02` | More lenient edge (2%) |

### Strategy 2 (Momentum) Tiers
| Seconds Left | Min Confidence | Budget Allocation |
|-------------|---------------|-------------------|
| 120–60s | 65% | 30% ($0.60) |
| 180–120s | 70% | 25% ($0.50) |
| 240–180s | 75% | 20% ($0.40) |

---

## Setup

```bash
# 1. Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 2. Build
cargo build --release

# 3. First run (prompts for private key + password)
cargo run --release

# 4. Subsequent runs (prompts for password only)
cargo run --release
```

### Environment Variables (`.env`)

```
ANKR_API_KEY=<your_ankr_key>
SIMPLESWAP_API_KEY=<your_simpleswap_key>   # optional, for POL gas top-up
```

## Execution Flow Diagram

```
Startup
  |
  +--> Decrypt private key (Argon2id + AES-256-GCM)
  +--> Setup wallet (LocalWallet + Polymarket API creds)
  +--> Ensure on-chain approvals (USDC + ERC-1155)
  +--> Redeem positions from prior windows
  +--> Preflight balance check:
  |      +--> POL <= 5? → Discord alert + HALT
  |      +--> USDC.e < $2? → Swap 80% POL → USDC.e (SimpleSwap)
  +--> Send Discord startup alert
  +--> Start Binance BTC price stream (background)
  |
  v
Main Loop (per 5-min window)
  |
  +--> Discover active btc-updown-5m market (Gamma API)
  +--> Reset BTC window-open price
  +--> Spawn fee rate polling (every 1s)
  +--> Spawn strategy evaluation (every 1s)
  +--> Connect Polymarket WS (streams UP/DOWN prices)
  |
  |   Strategy Loop (T-240s to T-8s, every 1s):
  |     |
  |     +--> Read BTC price, compute % change, dollar move
  |     +--> Check price staleness (<20s)
  |     +--> Check flat market filter (>0.015%)
  |     +--> Compute confidence (normal CDF on remaining vol)
  |     |
  |     +--> Try Strategy 1 "Impulse" (T-240s to T-8s):
  |     |      +--> Early: $60 floor + tiered confidence (70-80%) + PM drift + edge≥3%
  |     |      +--> Late:  %-based tiers + PM drift + edge≥3%
  |     |
  |     +--> Try Strategy 2 "Momentum" (T-240s to T-60s):
  |     |      +--> $65 floor + tiered confidence (65-75%) + edge≥2%
  |     |      +--> No PM drift check
  |     |
  |     +--> First signal wins → Place FAK order (EIP-712 signed)
  |     +--> Log [Impulse] or [Momentum] + Send Discord alert
  |     +--> Set bet_placed = true (max 1 per window)
  |
  +--> Window closes
  +--> Redeem prior window positions
  +--> Check/top-up POL gas
  +--> Loop to next window
```
