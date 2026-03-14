# polymarket-arb2

Polymarket BTCUSD 5-minute UP/DOWN binary market arbitrage bot on Polygon.

## Project Structure

```
src/
├── main.rs        — Clean orchestrator. Runs modules, no arb logic.
├── encrypt.rs     — Argon2id + AES-256-GCM private key encryption at rest.
├── getblock.rs    — GetBlock.io WebSocket → Chainlink BTC/USD live price feed.
├── polymarket.rs  — Polymarket CLOB API (REST + WS), wallet, orders, market discovery.
└── chain.rs       — Polygon on-chain helpers (approvals, balance, redemption, gas top-up).
```

## What Was Done

### 1. Encryption (`encrypt.rs`)
- First run: prompts for private key + password (min 8 chars, confirmed twice).
- Encrypts with **Argon2id** key derivation + **AES-256-GCM** authenticated encryption.
- Stored at `data/key.enc` as `[16-byte salt][12-byte nonce][ciphertext+tag]`.
- Subsequent runs: prompts only for password to decrypt.

### 2. BTC Live Price (`getblock.rs`)
- Connects to `wss://go.getblock.io/{GETBLOCK_API_KEY}` (Ethereum mainnet node).
- Subscribes to `newHeads` (new blocks ~12s), queries Chainlink BTC/USD aggregator (`latestAnswer()`) on each block.
- Maintains shared `BtcPriceState` with window-open price, latest price, and percent change.
- Auto-reconnects on error with 3s backoff.

### 3. Polymarket Integration (`polymarket.rs`)
- **Market discovery**: finds active `btc-updown-5m-{timestamp}` markets via Gamma API.
- **WebSocket**: subscribes to order book updates, tracks best bid/ask for UP and DOWN outcomes.
- **Auth**: L1 (EIP-712 ClobAuth) for API credential derivation, L2 (HMAC-SHA256) for authenticated calls.
- **Order functions** (kept for future use): EIP-712 signing, order building, placement, cancellation.
- **Fee rate** polling per market window.

### 4. On-Chain (`chain.rs`)
- USDC.e balance checks, ERC-20 approvals (CTF Exchange, Neg Risk adapters).
- ERC-1155 `setApprovalForAll` for ConditionalTokens.
- CTF position redemption for expired windows (last 5 windows).
- POL gas top-up via SimpleSwap (USDC.e → POL).

### 5. Main Loop (`main.rs`)
- Decrypt key → setup wallet → ensure approvals → start BTC price stream.
- Cycles through 5-minute market windows: discover → track prices → redeem → repeat.
- No arbitrage logic inside main — just orchestration.

### 6. Cleanup
- Removed the old flawed strategy (buying when UP+DOWN < 100c — never triggers).
- Removed Discord alerts, money management system (constants kept for later).
- Removed `PRIVATE_KEY` from `.env` (now encrypted at rest).

## Setup

```bash
# 1. Install Rust (if needed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 2. Build
cargo build --release

# 3. Run (first time — will ask for private key + password)
cargo run --release

# 4. Run (subsequent — will ask for password only)
cargo run --release
```

### Environment Variables (`.env`)

```
ANKR_API_KEY=<your_ankr_key>
SIMPLESWAP_API_KEY=<your_simpleswap_key>
GETBLOCK_API_KEY=<your_getblock_key>
```

## What To Do Next

### Arbitrage Strategy
The core arb logic is not yet implemented. The infrastructure is ready — BTC price streams in real-time, Polymarket order book updates flow via WebSocket, and order functions exist. What's needed:

1. **Define the strategy**: Compare Chainlink BTC price movement against Polymarket UP/DOWN prices. If BTC is clearly trending up but UP outcome is underpriced (or vice versa), place a bet.
2. **Entry signals**: Use `BtcPriceState::pct_change()` combined with order book depth to decide when to enter.
3. **Position sizing**: Determine how much USDC to risk per window based on confidence and balance.
4. **Exit / hedging**: Decide whether to hold until expiry or sell position if price reverses.

### Features to Re-enable
- **Discord alerts**: Webhook URLs are kept as constants in `main.rs`. Wire them up for trade notifications and error alerts.
- **Money management**: Track P&L across windows, set daily loss limits, manage bankroll.

### Improvements
- **Faster price feed**: GetBlock gives ~12s updates (per Ethereum block). Consider adding a CEX WebSocket (Binance, Coinbase) for sub-second BTC prices.
- **Order book analysis**: Use ask-side depth and spread to find optimal entry prices.
- **Backtesting**: Log historical BTC prices and Polymarket prices to `data/` for offline strategy testing.
- **Multiple markets**: Extend beyond BTCUSD 5-min to other timeframes or assets.
