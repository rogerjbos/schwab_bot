# Schwab Trading Bot 🤖💼

A Rust-powered trading assistant that connects Schwab (and Interactive Brokers) brokerage accounts to a Telegram control channel. The bot keeps an eye on configured equities, evaluates momentum signals, enforces strict risk checks, and places limit orders through the official Schwab API. Configuration lives in `symbols_config.json`, letting you tailor exposure, thresholds, and account routing per symbol.

## ✨ Features

- **Telegram Control Surface** – Start, stop, and monitor the bot directly from a private Telegram chat.
- **Schwab API Integration** – Uses the `schwab_api` crate for OAuth, market data, balances, positions, and order placement.
- **Realtime Market Snapshots** – Caches quotes for tracked symbols and derives prices from held positions when quotes lag.
- **Risk Management Layer** – Cash availability, position sizing (1% cap), price sanity (±20%), and minimum notional checks before every order.
- **Open-Order Visibility** – Status report surfaces in-flight Schwab orders, their status, side, size, and entry timestamps.
- **Async Architecture** – Tokio + async/await keep the trading loop responsive while awaiting Schwab and Telegram traffic.
- **Modular Shared Crate** – Reuses the local `telegram-bot` crate for consistent command handling across projects.

## 🚀 Quick Start

### Prerequisites

- **Rust** (stable) – Install via [rustup](https://rustup.rs/)
- **Schwab API Access** – Register an application in the [Schwab Developer Portal](https://beta-developer.schwab.com/)
- **Telegram Bot** – Create a bot via [@BotFather](https://t.me/BotFather)
- **PKCS12 Certificates** – Export Schwab cert bundle into `self_certs_dir/` (PEM key + cert)

### Installation

1. **Clone the workspace**
   ```bash
   git clone https://github.com/rogerjbos/schwab_bot.git
   cd schwab_bot
   ```

2. **Copy the environment template**
   ```bash
   cp .env.example .env
   # Fill in your Schwab + Telegram credentials
   ```

3. **Populate `.env`**
   ```env
   # Schwab OAuth Client
   SCHWAB_API_KEY=your_registered_app_key
   SCHWAB_API_SECRET=your_registered_app_secret

   # Telegram Control Bot
   SCHWAB_TELOXIDE_TOKEN=telegram_bot_token
   SCHWAB_BOT_CHAT_ID=your_chat_id

   # Optional overrides
   SCHWAB_ACCOUNT_HASH=account_hash_if_you_have_multiple
   RUST_LOG=info

   # IBKR (optional)
   IBKR_HOST="127.0.0.1"
   IBKR_PORT="4002"
   IBKR_CLIENT_ID="0"
   ```

4. **Authorize with Schwab (first run only)**
   - Place the OAuth credential cache at `~/.credentials/Schwab-rust.json` (created by the `schwab_api` crate during login).
   - Ensure `self_certs_dir/` holds `cert.pem` and `key.pem` for mutual TLS.
   - Run the standalone `schwab_api` example (`cargo run` inside `schwab_api/`) to complete the device authorization flow if you have not already.

5. **Configure tracked symbols**
   - Edit `symbols_config.json` with entries like:
     ```json
      [
      {
         "symbol": "VTI",
         "account_hash": "YOUR_SCHWAB_ACCOUNT_HASH (not the account number)",
         "entry_amount": 1000,
         "exit_amount": 1000,
         "entry_threshold": -0.02,
         "exit_threshold": 0.02
      },
      {
         "symbol": "AAPL",
         "api": "ibkr",
         "account_hash": "YOUR_IBKR_ACCOUNT_NUMBER (no hash needed for ibapi)",
         "entry_amount": 500,
         "exit_amount": 500,
         "entry_threshold": -0.01,
         "exit_threshold": 0.01
      }
      ]
     ```
   - `entry_amount` / `exit_amount` are USD notionals the bot converts to share quantity using the latest price.
   - Thresholds represent percent change triggers based on Schwab quote data.

6. **Build + run**
   ```bash
   cargo build --release
   cargo run
   ```

   On startup the bot sends “Schwab bot application started” to the configured chat and begins polling Telegram for commands.

## 🔧 Development

### Project Structure

```
schwab_bot/
├── src/
│   ├── main.rs                     # Application entry point & Telegram wiring
│   ├── schwab_pos.rs               # TradingBot implementation + Schwab integration & risk checks
│   └── schwab_execute_strategy.rs  # Signal generation and execution loop
├── symbols_config.json             # Runtime trading configuration
├── self_certs_dir/                 # TLS certificate/key pair for Schwab API
├── Cargo.toml                      # Dependencies and metadata
└── README.md                       # Project documentation
```

### Useful Commands

```bash
# Debug build
cargo build

# Optimized build
cargo build --release

# Run with verbose logging
RUST_LOG=debug cargo run

# Format source (stable toolchain)
cargo fmt
```

## 🏗️ Architecture

### Core Components

- **`TradingBot`** – Manages Schwab API client, account caches, positions, quotes, and risk evaluation.
- **`TelegramBotHandler`** – Reusable handler that maps Telegram commands to bot actions.
- **Risk Engine** – `evaluate_order_risks` enforces cash coverage, position caps, sell availability, price deviation, and minimum notional before orders go out.
- **Status Reporter** – Compiles balances, positions, tracked symbol signals, and open Schwab orders into a Telegram-friendly report.

### Dependencies

| Crate | Purpose |
|-------|---------|
| `schwab_api` | Official Schwab REST + OAuth bindings |
| `teloxide` | Telegram Bot API client |
| `telegram-bot` | Local crate providing generic command routing |
| `tokio` | Async runtime |
| `reqwest` | HTTP client for fallback balance/position calls |
| `serde` / `serde_json` | Configuration + API payload handling |
| `env_logger` / `log` | Structured logging |
| `chrono` | Time handling for order windows & reporting |

## 🔒 Security & Risk Controls

- **Credential Isolation** – Secrets stay in `.env` and `~/.credentials/Schwab-rust.json`; both are gitignored.
- **Mutual TLS** – `self_certs_dir` holds Schwab-issued certificates required for API access.
- **Pre-Trade Checks** – Orders fail fast if cash is insufficient, size breaches 1% portfolio cap, sell quantity exceeds holdings, price drifts >20%, or notional is under $10.
- **Graceful Errors** – Failures surface in logs and Telegram without leaking sensitive data.

## 📱 Telegram Commands

The command set mirrors other bots built on the shared handler:

- `/start` – Initialize the bot and confirm responsiveness
- `/status` – Send balance, position, tracked signal, and open-order report
- `/execute` – Run the strategy loop once
- `/positions` – List cached Schwab positions
- `/stop` – Halt the bot loop safely

*(Customize or extend commands inside the `telegram-bot` crate.)*

## 🔧 Configuration Reference

### Environment Variables

| Variable | Description | Required |
|----------|-------------|----------|
| `SCHWAB_API_KEY` | Schwab Developer app key | Yes |
| `SCHWAB_API_SECRET` | Schwab Developer app secret | Yes |
| `SCHWAB_TELOXIDE_TOKEN` | Telegram bot token | Yes |
| `SCHWAB_BOT_CHAT_ID` | Target chat ID for notifications | Yes |
| `SCHWAB_ACCOUNT_HASH` | Preferred account hash (fallback auto-detected) | No |
| `RUST_LOG` | Logging level (`error`, `warn`, `info`, `debug`, `trace`) | No |

### Schwab Setup Checklist

1. Register an app in the Schwab Developer Portal and download the certificate bundle.
2. Convert certificates to PEM and place at `self_certs_dir/cert.pem` and `self_certs_dir/key.pem`.
3. Run the companion `schwab_api` project to obtain the OAuth credential JSON at `~/.credentials/Schwab-rust.json`.
4. Copy the displayed account hash (or fetch via API) into `symbols_config.json` per symbol.

### Telegram Setup Checklist

1. Use [@BotFather](https://t.me/BotFather) to create a new bot; save the token.
2. Add the bot to a private chat or group; grab the chat ID via [@userinfobot](https://t.me/userinfobot).
3. Paste both values into `.env` before running the bot.

## 🚨 Error Handling & Observability

- **Logging** – Controlled through `RUST_LOG`; defaults to `info`.
- **Retries** – Schwab API calls rely on the underlying crate’s retry/backoff policies.
- **Telemetry** – Status command doubles as a heartbeat, providing real-time account + order visibility.

## 🤝 Contributing

1. Fork the repository
2. Create a feature branch: `git checkout -b feature/some-improvement`
3. Implement and test your changes
4. Format code: `cargo fmt`
5. Run builds/tests as appropriate: `cargo build`, `cargo test`
6. Commit with context: `git commit -m "Describe your change"`
7. Push the branch and open a Pull Request

## 📜 License & Disclaimer

This bot is provided for educational purposes only. Trading carries substantial risk, including the possible loss of principal. You are solely responsible for any trades executed with this software. Review Schwab’s API terms and your broker agreement before enabling live trading.

## 🔗 Related Resources

- [`schwab_api` crate](../schwab_api/) – OAuth + REST helper utilities used by this bot
- [`telegram-bot` crate](../telegram-bot/) – Shared Telegram command framework
- [Schwab API Documentation](https://beta-developer.schwab.com/products/trader-api)
- [Teloxide Documentation](https://docs.rs/teloxide/latest/teloxide/)

---

**Built with 🦀 Rust and a healthy respect for risk management.**
