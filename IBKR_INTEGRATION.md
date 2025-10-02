# IBKR Integration Status

## Overview
This branch (`ibkr_integration`) adds IBKR API support to the Schwab trading bot, enabling per-symbol routing between Schwab and Interactive Brokers.

## What's Implemented

### ✅ Completed
1. **Data Structures**
   - Added `api: Option<String>` field to `Symbol` and `SymbolConfig` structs
   - Symbols can now specify `"api": "ibkr"` in `symbols_config.json` to route to IB

2. **IBKR Client Integration**
   - Added `ib_client: Option<Arc<ibapi::Client>>` to `TradingBot`
   - Implemented lazy connection via `ensure_ib_client_connected()`
   - Connects using env vars: `IBKR_HOST`, `IBKR_PORT`, `IBKR_CLIENT_ID`
   - Switches to delayed market data automatically

3. **Smart Account Detection**
   - **Before**: Used heuristics (account hash length < 20 chars = IBKR)
   - **Now**: Uses symbol configuration - if any symbol in an account uses "ibkr" API, the whole account uses IBKR
   - Accounts can be mixed in the same config file

4. **IBKR Position Fetching** ✨ NEW
   - `update_positions()` now detects IBKR vs Schwab accounts automatically
   - IBKR accounts (short alphanumeric IDs like "DUH84268") fetch via `fetch_ib_positions()`
   - Schwab accounts (long hex hashes) fetch via Schwab API
   - Uses `ibapi::accounts::AccountUpdate` stream to get portfolio positions

5. **Order Routing**
   - `send_limit_order()` now routes based on `symbol.api` field
   - Routes to IBKR if `api == "ibkr"`, otherwise routes to Schwab (default)

5. **Schwab Commission Policy**
   - Added `estimate_schwab_commission()` method (reads `SCHWAB_COMMISSION_FLAT` env var)
   - Implements Policy B: blocks Schwab orders if estimated commission > 0

6. **Risk Management - Daily Order Limits**
   - Added `order_history` field to track last order timestamp per symbol
   - Enforces one order per symbol per day rule
   - Orders are blocked if a symbol has already been traded today

7. **Background Tasks**
   - Placeholder for IB order update subscription (positions & commission tracking)

### ⚠️ TODO / Not Yet Implemented

1. **IBKR Order Placement**
   - Currently returns an error with message: "IBKR routing configured but order placement not yet fully implemented"
   - Need to construct proper `ibapi::orders::Order` objects
   - The ibapi library's Order API is complex - needs careful implementation
   - Reference: see `/Users/rogerbos/rust_home/ibkr_bot/src/ibkr_pos.rs` for working examples

2. **IBKR Order Update Stream**
   - Background task is spawned but currently just logs a placeholder message
   - Need to implement actual order update subscription using `ibapi::orders`
   - Should process `ExecutionData` and `CommissionReport` to update local caches

## Usage

### Environment Variables
```bash
# Schwab (required)
export SCHWAB_API_KEY="your_schwab_client_id"
export SCHWAB_API_SECRET="your_schwab_client_secret"

# IBKR (optional - only needed if routing symbols to IBKR)
export IBKR_HOST="127.0.0.1"          # default
export IBKR_PORT="4001"                # 4001=live, 7497=paper
export IBKR_CLIENT_ID="0"              # default

# Commission policy (optional)
export SCHWAB_COMMISSION_FLAT="0.0"    # flat commission per order
```

### symbols_config.json Example
```json
[
  {
    "symbol": "VTI",
    "account_hash": "YOUR_SCHWAB_ACCOUNT_HASH",
    "entry_amount": 1000,
    "exit_amount": 1000,
    "entry_threshold": -0.02,
    "exit_threshold": 0.02
  },
  {
    "symbol": "AAPL",
    "api": "ibkr",
    "account_hash": "YOUR_SCHWAB_ACCOUNT_HASH",
    "entry_amount": 500,
    "exit_amount": 500,
    "entry_threshold": -0.01,
    "exit_threshold": 0.01
  }
]
```

## Testing

### Current Status
✅ Bot compiles and starts successfully  
✅ Schwab token initialization works (no more "Token error: Server returned error response")  
✅ Per-symbol routing logic is in place  
✅ IBKR position fetching works automatically  
⚠️ IBKR order placement will return error message (not yet implemented)  
⚠️ IBKR order updates not yet processed  

### Next Steps for Full Implementation
1. Study the `ibapi` crate's Order construction API
2. Implement proper Order creation in `send_limit_order()` for IBKR path
3. Implement order update stream processing in `ensure_ib_client_connected()`
4. Add IBKR position fetching
5. Test with paper trading account first

## Branch History
- Created from `master` (working Schwab-only bot)
- Previous `feat/ibkr_trades` branch had token initialization issues
- This clean reimplementation from master avoids those problems

## Files Modified
- `Cargo.toml` - Added `ibapi` and `tokio-stream` dependencies
- `src/schwab_pos.rs` - Main integration changes

## Dependencies Added
```toml
ibapi = { git = "https://github.com/wboayue/rust-ibapi", features = ["async"] }
tokio-stream = "0.1"
```
