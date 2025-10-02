use async_trait::async_trait;
use chrono;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    io,
    path::PathBuf,
    sync::Arc,
};

use telegram_bot::BotState;
use teloxide::{prelude::*, types::ChatId};
use tokio::sync::Mutex;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

// Schwab API imports
use schwab_api::api::Api;
use schwab_api::error::Error as SchwabError;
use schwab_api::model::market_data::quote_response::QuoteResponse;
use schwab_api::model::{Instruction, InstrumentRequest, OrderRequest};
use schwab_api::token::{TokenChecker, Tokener};

use crate::schwab;
use crate::ibkr;
use crate::models::{Symbol, PositionSummary, RealTimeQuote, SignalInfo};

/// Main trading bot implementation with Schwab API integration.
pub struct TradingBot {
    pub api: Option<Api<SharedTokenChecker>>,
    pub tokener: SharedTokenChecker,
    pub account_hashes: Vec<String>,
    // Optional IBKR client for routing trades/holdings
    pub ib_client: Option<Arc<ibapi::Client>>,
    pub positions: Arc<Mutex<HashMap<String, HashMap<String, f64>>>>,
    real_time_prices: Arc<Mutex<HashMap<String, RealTimeQuote>>>,
    pub symbols_config: Vec<Symbol>,
    pub account_balances: Arc<Mutex<HashMap<String, f64>>>,
    // Track last order date per symbol for daily order limit
    order_history: Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,
}

#[derive(Clone)]
pub(crate) struct SharedTokenChecker {
    inner: Arc<TokenChecker>,
}

impl SharedTokenChecker {
    async fn new(
        path: PathBuf,
        client_id: String,
        secret: String,
        redirect_url: String,
        certs_dir: PathBuf,
    ) -> Result<Self, SchwabError> {
        let checker = TokenChecker::new(path, client_id, secret, redirect_url, certs_dir).await?;
        Ok(Self {
            inner: Arc::new(checker),
        })
    }
}

impl Tokener for SharedTokenChecker {
    fn get_access_token(
        &self,
    ) -> impl std::future::Future<Output = Result<String, SchwabError>> + Send {
        let inner = Arc::clone(&self.inner);
        async move { Tokener::get_access_token(&*inner).await }
    }

    fn redo_authorization(
        &self,
    ) -> impl std::future::Future<Output = Result<(), SchwabError>> + Send {
        let inner = Arc::clone(&self.inner);
        async move { Tokener::redo_authorization(&*inner).await }
    }
}

impl TradingBot {
    /// Creates a new instance of the TradingBot with Schwab API integration.
    pub async fn new() -> Result<Self, Box<dyn Error + Send + Sync>> {
        // Read Schwab API credentials from environment
        let key = std::env::var("SCHWAB_API_KEY")?;
        let secret = std::env::var("SCHWAB_API_SECRET")?;

        // Initialize Schwab API client
        let callback_url = "https://127.0.0.1:8080".to_string();
        let path = dirs::home_dir()
            .expect("home dir")
            .join(".credentials")
            .join("Schwab-rust.json");
        let certs_dir = PathBuf::from("./self_certs_dir");
        let shared_tokener =
            SharedTokenChecker::new(path, key, secret, callback_url, certs_dir).await?;

        let api = Api::new(shared_tokener.clone())
            .await
            .map_err(|e| format!("Failed to create API client: {}", e))?;

        // Read and parse the symbols configuration from a file
        let raw_symbols =
            Self::read_symbols_config("/Users/rogerbos/rust_home/schwab_bot/symbols_config.json")
                .await?;

        let mut symbols = Vec::new();
        let mut account_hash_set = HashSet::new();

        for symbol in raw_symbols {
            account_hash_set.insert(symbol.account_hash.clone());
            symbols.push(symbol);
        }

        let mut account_hashes: Vec<String> = account_hash_set.into_iter().collect();
        if account_hashes.is_empty() {
            let fallback_hash = match std::env::var("SCHWAB_ACCOUNT_HASH") {
                Ok(hash) => hash,
                Err(_) => {
                    let request = api
                        .get_account_numbers()
                        .await
                        .map_err(|e| format!("Failed to request account numbers: {}", e))?;
                    let accounts = request
                        .send()
                        .await
                        .map_err(|e| format!("Failed to fetch account numbers: {}", e))?;

                    accounts
                        .first()
                        .map(|acct| acct.hash_value.clone())
                        .ok_or_else(|| "Schwab API returned no account numbers".to_string())?
                }
            };
            account_hashes.push(fallback_hash);
        } else {
            account_hashes.sort();
        }

        // Connect IBKR client if any symbols require it
        let needs_ib = symbols.iter().any(|s| s.api.as_deref() == Some("ibkr"));
        let ib_client = if needs_ib {
            let host = std::env::var("IBKR_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
            let port: u16 = std::env::var("IBKR_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(4002);
            let client_id: i32 = std::env::var("IBKR_CLIENT_ID").ok().and_then(|s| s.parse().ok()).unwrap_or(100);

            let endpoint = format!("{}:{}", host, port);

            let client = match ibapi::Client::connect(endpoint.as_str(), client_id).await {
                Ok(c) => c,
                Err(e) => return Err(format!("Failed to connect IB client: {}", e).into()),
            };

            // probably won't end up using IBKR market data
            // Optionally request delayed market data for this session
            if let Err(e) = client.switch_market_data_type(ibapi::market_data::MarketDataType::Delayed).await {
                eprintln!("Warning: failed to switch market data type to Delayed: {}", e);
            }

            Some(Arc::new(client))
        } else {
            None
        };

        // If we have an IB client, attempt to pre-populate today's order history from IBKR
        // We'll synchronously fetch today's executions and open/completed orders during init so
        // the in-memory `order_history` prevents duplicate orders across restarts.
        let mut initial_order_history: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        // Populate IBKR history first
        if let Some(ref client) = ib_client {
            match Self::populate_order_history_from_ibkr(client, Arc::new(Mutex::new(HashMap::new()))).await {
                Ok(map) => initial_order_history.extend(map.into_iter()),
                Err(e) => eprintln!("Warning: failed to pre-populate IBKR order history: {}", e),
            }
        }

        // Populate Schwab order history for configured accounts (block on lookup)
        match Self::populate_order_history_from_schwab(&api, &shared_tokener, &symbols).await {
            Ok(map) => initial_order_history.extend(map.into_iter()),
            Err(e) => eprintln!("Warning: failed to pre-populate Schwab order history: {}", e),
        }

        Ok(Self {
            api: Some(api),
            tokener: shared_tokener,
            account_hashes,
            ib_client,
            positions: Arc::new(Mutex::new(HashMap::new())),
            real_time_prices: Arc::new(Mutex::new(HashMap::new())),
            symbols_config: symbols,
            account_balances: Arc::new(Mutex::new(HashMap::new())),
            order_history: Arc::new(Mutex::new(initial_order_history)),
        })
    }

    /// Query IBKR for today's executions and open/completed orders and return a map of
    /// symbol -> latest execution/filled timestamp for today. This helps prevent duplicate
    /// orders for the same symbol in the same day after a restart.
    async fn populate_order_history_from_ibkr(
        client: &ibapi::Client,
        _sink: Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,
    ) -> Result<HashMap<String, chrono::DateTime<chrono::Utc>>, Box<dyn std::error::Error + Send + Sync>> {
        ibkr::populate_order_history_from_ibkr(client).await
    }

    /// Query Schwab for recent orders across configured accounts and return a map of symbol->timestamp
    async fn populate_order_history_from_schwab(
        api: &Api<SharedTokenChecker>,
        tokener: &SharedTokenChecker,
        symbols: &Vec<Symbol>,
    ) -> Result<HashMap<String, chrono::DateTime<chrono::Utc>>, Box<dyn std::error::Error + Send + Sync>> {
        schwab::populate_order_history_from_schwab(tokener, api, symbols).await
    }

    /// Fetch positions from IBKR for a given account
    async fn fetch_ib_positions(
        &self,
        account_id: &str,
    ) -> Result<Vec<PositionSummary>, Box<dyn std::error::Error + Send + Sync>> {
        ibkr::fetch_ib_positions(self, account_id).await
    }

    /// Fetch balance from IBKR for a given account
    async fn fetch_ib_balance(
        &self,
        account_id: &str,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        ibkr::fetch_ib_balance(self, account_id).await
    }

    async fn fetch_positions(
        tokener: &SharedTokenChecker,
        account_hash: &str,
    ) -> Result<Vec<PositionSummary>, Box<dyn std::error::Error + Send + Sync>> {
        schwab::fetch_positions_schwab(tokener, account_hash).await
    }

    async fn get_balance(
        api: &Api<SharedTokenChecker>,
        account_hash: &str,
    ) -> Result<Vec<f64>, Box<dyn std::error::Error + Send + Sync>> {
        schwab::get_balance_schwab(api, account_hash).await
    }

    /// Update account information from Schwab API
    pub async fn update_account_info(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let api = self.api.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "Schwab API client not initialized")
        })?;

        let mut any_success = false;
        let mut aggregated_balances = HashMap::new();

        for account_hash in &self.account_hashes {
            // Determine API based on symbols configured for this account
            let account_symbols: Vec<&Symbol> = self.symbols_config.iter()
                .filter(|s| s.account_hash == *account_hash)
                .collect();

            let uses_ibkr = account_symbols.iter().any(|s| s.api.as_deref() == Some("ibkr"));

            if uses_ibkr {
                // Fetch from IBKR
                match self.fetch_ib_balance(account_hash).await {
                    Ok(balance) => {
                        aggregated_balances.insert(account_hash.clone(), balance);
                        any_success = true;
                    }
                    Err(err) => {
                        eprintln!("⚠️ Unable to fetch IBKR balance for {}: {}", account_hash, err);
                    }
                }
            } else {
                match Self::get_balance(api, account_hash).await {
                    Ok(values) => {
                        if values.is_empty() {
                            // Empty balance payload
                        } else {
                            let primary_value = values.get(if values.len() == 3 { 2 } else { 1 }).copied().unwrap_or_else(|| values.last().copied().unwrap_or(0.0));

                            aggregated_balances.insert(account_hash.clone(), primary_value);
                            any_success = true;
                        }
                    }
                    Err(err) => {
                        eprintln!("⚠️ Unable to fetch balances for {}: {}", account_hash, err);
                    }
                }
                }
            }

        if !any_success {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Balance payloads were empty for all configured accounts",
            )
            .into());
        }

        let mut balances_guard = self.account_balances.lock().await;
        *balances_guard = aggregated_balances;

        Ok(())
    }

    /// Update positions from Schwab API or IBKR
    pub async fn update_positions(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut aggregated_positions: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut derived_quotes: HashMap<String, RealTimeQuote> = HashMap::new();
        let mut any_success = false;

        for account_hash in &self.account_hashes.clone() {
            // Determine API based on symbols configured for this account
            let account_symbols: Vec<&Symbol> = self.symbols_config.iter()
                .filter(|s| s.account_hash == *account_hash)
                .collect();

            let uses_ibkr = account_symbols.iter().any(|s| s.api.as_deref() == Some("ibkr"));

            if uses_ibkr {
                // Fetch from IBKR
                match self.fetch_ib_positions(account_hash).await {
                    Ok(position_details) => {
                        let account_entry = aggregated_positions
                            .entry(account_hash.clone())
                            .or_insert_with(HashMap::new);
                        account_entry.clear();
                        
                        for pos in &position_details {
                            account_entry.insert(pos.symbol.clone(), pos.quantity);

                            if let (Some(market_value), quantity) = (pos.market_value, pos.quantity) {
                                if quantity.abs() > f64::EPSILON {
                                    let last_price = market_value / quantity.abs();
                                    derived_quotes
                                        .entry(pos.symbol.clone())
                                        .and_modify(|quote| quote.last_price = Some(last_price))
                                        .or_insert_with(|| RealTimeQuote {
                                            last_price: Some(last_price),
                                            ..Default::default()
                                        });
                                }
                            }
                        }

                        any_success = true;
                    }
                    Err(err) => {
                        eprintln!("⚠️ Unable to fetch IBKR positions for {}: {}", account_hash, err);
                    }
                }
            } else {
                // Fetch from Schwab
                match Self::fetch_positions(&self.tokener, &account_hash).await {
                    Ok(position_details) => {
                        let account_entry = aggregated_positions
                            .entry(account_hash.clone())
                            .or_insert_with(HashMap::new);
                        account_entry.clear();
                        for pos in &position_details {
                            account_entry.insert(pos.symbol.clone(), pos.quantity);

                            if let (Some(market_value), quantity) = (pos.market_value, pos.quantity) {
                                if quantity.abs() > f64::EPSILON {
                                    let last_price = market_value / quantity.abs();
                                    derived_quotes
                                        .entry(pos.symbol.clone())
                                        .and_modify(|quote| quote.last_price = Some(last_price))
                                        .or_insert_with(|| RealTimeQuote {
                                            last_price: Some(last_price),
                                            ..Default::default()
                                        });
                                }
                            }
                        }

                        any_success = true;
                    }
                    Err(err) => {
                        eprintln!("⚠️ Unable to fetch Schwab positions for {}: {}", account_hash, err);
                    }
                }
            }
        }

        if !any_success {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Unable to fetch positions for all configured accounts",
            )
            .into());
        }

        {
            let mut positions_guard = self.positions.lock().await;
            *positions_guard = aggregated_positions;
        }

        {
            let mut prices_guard = self.real_time_prices.lock().await;
            for (symbol, quote) in derived_quotes {
                let entry = prices_guard
                    .entry(symbol)
                    .or_insert_with(RealTimeQuote::default);
                if let Some(price) = quote.last_price {
                    entry.last_price = Some(price);
                }
            }
        }

        Ok(())
    }

    /// Update current prices for all symbols
    pub async fn update_prices(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(ref api) = self.api {
            let mut seen = HashSet::new();
            let mut new_quotes: Vec<(String, RealTimeQuote)> = Vec::new();

            for symbol in &self.symbols_config {
                if !seen.insert(symbol.symbol.clone()) {
                    continue;
                }

                match api.get_quote(symbol.symbol.clone()).await {
                    Ok(req) => match req.send().await {
                        Ok(response) => {
                            let mut quote_record = RealTimeQuote::default();

                            match response {
                                QuoteResponse::Equity(equity_box) => {
                                    let equity = equity_box.as_ref();

                                    if let Some(regular) = equity.regular {
                                        quote_record.last_price = Some(regular.last_price);
                                        quote_record.percent_change = regular.percent_change;
                                    }

                                    let quote = &equity.quote;
                                    if quote_record.last_price.is_none() {
                                        quote_record.last_price = Some(quote.last_price);
                                    }
                                    if quote_record.percent_change.is_none() {
                                        quote_record.percent_change = quote.net_percent_change;
                                    }

                                    quote_record.total_volume = Some(quote.total_volume as f64);

                                    if let Some(fundamental) = equity.fundamental {
                                        quote_record.avg_10_day_volume =
                                            Some(fundamental.avg_10_days_volume);
                                    }
                                }
                                QuoteResponse::Option(option_box) => {
                                    let option_quote = option_box.as_ref();
                                    let quote = &option_quote.quote;

                                    quote_record.last_price = Some(quote.last_price);
                                    quote_record.total_volume = Some(quote.total_volume as f64);
                                    quote_record.percent_change = Some(quote.net_percent_change);
                                }
                                _ => {}
                            }

                            if quote_record.last_price.is_some()
                                || quote_record.total_volume.is_some()
                                || quote_record.percent_change.is_some()
                                || quote_record.avg_10_day_volume.is_some()
                            {
                                new_quotes.push((symbol.symbol.clone(), quote_record));
                            }
                        }
                        Err(e) => eprintln!("Failed to get quote for {}: {}", symbol.symbol, e),
                    },
                    Err(e) => eprintln!(
                        "Failed to create quote request for {}: {}",
                        symbol.symbol, e
                    ),
                }
            }

            if !new_quotes.is_empty() {
                let mut prices = self.real_time_prices.lock().await;
                for (symbol, quote_record) in new_quotes {
                    let entry = prices.entry(symbol).or_insert_with(RealTimeQuote::default);
                    if let Some(last_price) = quote_record.last_price {
                        entry.last_price = Some(last_price);
                    }
                    if let Some(volume) = quote_record.total_volume {
                        entry.total_volume = Some(volume);
                    }
                    if let Some(percent_change) = quote_record.percent_change {
                        entry.percent_change = Some(percent_change);
                    }
                    if let Some(avg_volume) = quote_record.avg_10_day_volume {
                        entry.avg_10_day_volume = Some(avg_volume);
                    }
                }
            }
        }
        Ok(())
    }

    /// Generate a status report
    pub async fn generate_status_report(&mut self) -> String {
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut report = format!("📊 Schwab Bot - {}\n\n", timestamp);

        if self.account_hashes.is_empty() {
            report.push_str("⚠️ No account hashes are configured in symbols_config.json.\n");
            return report;
        }

        report.push_str("Balance Summary:\n");
        if let Some(api) = self.api.as_ref() {
            let mut balance_snapshot = HashMap::new();

            for account_hash in &self.account_hashes {
                let label = if account_hash.len() > 8 {
                    format!(
                        "{}…{}",
                        &account_hash[..4],
                        &account_hash[account_hash.len() - 4..]
                    )
                } else {
                    account_hash.clone()
                };

                report.push_str(&format!("Account {}:\n", label));

                match Self::get_balance(api, account_hash).await {
                    Ok(balances) if balances.len() == 2 => {
                        report.push_str(&format!(
                            "Cash Available for Trading: ${:.2}\n",
                            balances[0]
                        ));
                        report.push_str(&format!(
                            "Liquidation Value:           ${:.2}\n",
                            balances[1]
                        ));
                        balance_snapshot.insert(account_hash.clone(), balances[1]);
                    }
                    Ok(balances) if balances.len() == 3 => {
                        report.push_str(&format!("Available Funds: ${:.2}\n", balances[0]));
                        report.push_str(&format!("Buying Power:    ${:.2}\n", balances[1]));
                        report.push_str(&format!("Equity:          ${:.2}\n", balances[2]));
                        balance_snapshot.insert(account_hash.clone(), balances[2]);
                    }
                    Ok(other) => {
                        report.push_str("⚠️ Received unexpected balance payload from Schwab.\n");
                        if let Some(last) = other.last() {
                            balance_snapshot.insert(account_hash.clone(), *last);
                        }
                    }
                    Err(err) => {
                        report.push_str(&format!("⚠️ Unable to fetch balances: {}\n", err));
                    }
                }

                report.push_str("\n");
            }

            if !balance_snapshot.is_empty() {
                let mut balances_guard = self.account_balances.lock().await;
                *balances_guard = balance_snapshot;
            }
        } else {
            report.push_str("  ⚠️ Schwab API client not initialized.\n\n");
        }

        for account_hash in self.account_hashes.clone() {
            let label = if account_hash.len() > 8 {
                format!(
                    "{}…{}",
                    &account_hash[..4],
                    &account_hash[account_hash.len() - 4..]
                )
            } else {
                account_hash.clone()
            };

            report.push_str(&format!("Positions for account {}:\n", label));

            // Determine API based on symbols configured for this account
            let account_symbols: Vec<&Symbol> = self.symbols_config.iter()
                .filter(|s| s.account_hash == *account_hash)
                .collect();

            let uses_ibkr = account_symbols.iter().any(|s| s.api.as_deref() == Some("ibkr"));

            if uses_ibkr {
                // For IBKR accounts, try to fetch from IBKR if client is available
                if self.ib_client.is_some() {
                    match self.fetch_ib_positions(&account_hash).await {
                        Ok(position_details) => {
                            {
                                let mut positions_guard = self.positions.lock().await;
                                let entry = positions_guard
                                    .entry(account_hash.clone())
                                    .or_insert_with(HashMap::new);
                                entry.clear();
                                for pos in &position_details {
                                    entry.insert(pos.symbol.clone(), pos.quantity);
                                }
                            }

                            if position_details.is_empty() {
                                report.push_str("(No Open Positions)\n\n");
                            } else {
                                report.push_str("Symbol Qty      AvgPrice MV     Cost   P / L  \n");
                                report.push_str("------ -------- -------- ------ ------ ------ \n");

                                let mut total_market_value = 0.0;
                                let mut total_profit_loss = 0.0;

                                for pos in &position_details {
                                    let side = if pos.is_short { " S" } else { " L" };
                                    let quantity_display = format!("{:.0}{}", pos.quantity, side);
                                    let avg_price = pos
                                        .average_price
                                        .map(|v| format!("{:.2}", v))
                                        .unwrap_or_else(|| "-".to_string());
                                    let market_value = pos
                                        .market_value
                                        .map(|v| {
                                            total_market_value += v;
                                            format!("{:.0}", v)
                                        })
                                        .unwrap_or_else(|| "-".to_string());
                                    let cost_basis = pos
                                        .cost_basis
                                        .map(|v| format!("{:.0}", v))
                                        .unwrap_or_else(|| "-".to_string());
                                    let profit_loss = pos
                                        .profit_loss
                                        .map(|v| {
                                            total_profit_loss += v;
                                            format!("{:.0}", v)
                                        })
                                        .unwrap_or_else(|| "-".to_string());

                                    let short_symbol: String = pos.symbol.chars().take(5).collect();

                                    report.push_str(&format!(
                                        "{:<6} {:>8} {:>8} {:>6} {:>6} {:>6} \n",
                                        short_symbol,
                                        quantity_display,
                                        avg_price,
                                        market_value,
                                        cost_basis,
                                        profit_loss
                                    ));
                                }

                                report.push_str(
                                    "-----------------------------------------------------------\n",
                                );
                                report
                                    .push_str(&format!("Total Market Value: ${:.2}\n", total_market_value));
                                report.push_str(&format!(
                                    "Total P/L:          ${:.2}\n\n",
                                    total_profit_loss
                                ));
                            }
                        }
                        Err(err) => {
                            report.push_str(&format!("  ⚠️ Unable to fetch IBKR positions: {}\n\n", err));
                        }
                    }
                } else {
                    report.push_str("  ⚠️ IBKR client not connected (missing environment variables)\n\n");
                }
            } else {
                // For Schwab accounts, fetch from Schwab API
                match Self::fetch_positions(&self.tokener, &account_hash).await {
                Ok(position_details) => {
                    {
                        let mut positions_guard = self.positions.lock().await;
                        let entry = positions_guard
                            .entry(account_hash.clone())
                            .or_insert_with(HashMap::new);
                        entry.clear();
                        for pos in &position_details {
                            entry.insert(pos.symbol.clone(), pos.quantity);
                        }
                    }

                    if position_details.is_empty() {
                        report.push_str("(No Open Positions)\n\n");
                    } else {
                        report.push_str("Symbol Qty      AvgPrice MV     Cost   P / L  \n");
                        report.push_str("------ -------- -------- ------ ------ ------ \n");

                        let mut total_market_value = 0.0;
                        let mut total_profit_loss = 0.0;

                        for pos in &position_details {
                            let side = if pos.is_short { " S" } else { " L" };
                            let quantity_display = format!("{:.0}{}", pos.quantity, side);
                            let avg_price = pos
                                .average_price
                                .map(|v| format!("{:.2}", v))
                                .unwrap_or_else(|| "-".to_string());
                            let market_value = pos
                                .market_value
                                .map(|v| {
                                    total_market_value += v;
                                    format!("{:.0}", v)
                                })
                                .unwrap_or_else(|| "-".to_string());
                            let cost_basis = pos
                                .cost_basis
                                .map(|v| format!("{:.0}", v))
                                .unwrap_or_else(|| "-".to_string());
                            let profit_loss = pos
                                .profit_loss
                                .map(|v| {
                                    total_profit_loss += v;
                                    format!("{:.0}", v)
                                })
                                .unwrap_or_else(|| "-".to_string());

                            let short_symbol: String = pos.symbol.chars().take(5).collect();

                            report.push_str(&format!(
                                "{:<6} {:>8} {:>8} {:>6} {:>6} {:>6} \n",
                                short_symbol,
                                quantity_display,
                                avg_price,
                                market_value,
                                cost_basis,
                                profit_loss
                            ));
                        }

                        report.push_str(
                            "-----------------------------------------------------------\n",
                        );
                        report
                            .push_str(&format!("Total Market Value: ${:.2}\n", total_market_value));
                        report.push_str(&format!(
                            "Total P/L:          ${:.2}\n\n",
                            total_profit_loss
                        ));
                    }
                }
                Err(err) => {
                    report.push_str(&format!("  ⚠️ Unable to fetch positions: {}\n\n", err));
                }
            }
        }
    }

        if let Some(api) = self.api.as_ref() {
            let lookback_end = chrono::Utc::now();
            let lookback_start = lookback_end - chrono::Duration::hours(24);
            let mut any_open_orders = false;

            report.push_str("Open Orders (last 24h):\n");

            for account_hash in &self.account_hashes {
                let label = if account_hash.len() > 8 {
                    format!(
                        "{}…{}",
                        &account_hash[..4],
                        &account_hash[account_hash.len() - 4..]
                    )
                } else {
                    account_hash.clone()
                };

                match api
                    .get_account_orders(
                        account_hash.clone(),
                        lookback_start.clone(),
                        lookback_end.clone(),
                    )
                    .await
                {
                    Ok(request) => match request.send().await {
                        Ok(orders) => {
                            let mut account_section = format!("Account {}:\n", label);
                            let mut account_has_open = false;

                            for order in orders {
                                let status_label = format!("{:?}", order.status);
                                let is_closed = matches!(
                                    status_label.as_str(),
                                    "Filled"
                                        | "Cancelled"
                                        | "Canceled"
                                        | "Expired"
                                        | "Rejected"
                                        | "Executed"
                                        | "Complete"
                                        | "Completed"
                                );

                                if is_closed {
                                    continue;
                                }

                                let order_json = serde_json::to_value(&order).unwrap_or(Value::Null);

                                let mut leg_note = String::new();
                                let (symbol_text, instruction_text, qty_value) = order_json
                                    .get("orderLegCollection")
                                    .and_then(|legs| legs.as_array())
                                    .map(|legs| {
                                        if legs.len() > 1 {
                                            leg_note = format!(" (+{} legs)", legs.len() - 1);
                                        }

                                        let first_leg = legs.first().cloned().unwrap_or(Value::Null);
                                        let symbol = first_leg
                                            .get("instrument")
                                            .and_then(|inst| inst.get("symbol"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("-")
                                            .to_string();
                                        let instruction = first_leg
                                            .get("instruction")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("-")
                                            .to_string();
                                        let qty = first_leg
                                            .get("quantity")
                                            .and_then(|v| v.as_f64())
                                            .unwrap_or(0.0);

                                        (symbol, instruction, qty)
                                    })
                                    .unwrap_or_else(|| ("-".to_string(), "-".to_string(), 0.0));

                                let qty_text = if qty_value.abs() > f64::EPSILON {
                                    format!("{:.0}", qty_value)
                                } else {
                                    "-".to_string()
                                };

                                let price_text = order_json
                                    .get("price")
                                    .and_then(|v| v.as_f64())
                                    .map(|price| format!(" @ ${:.2}", price))
                                    .unwrap_or_default();

                                let entered_local = order
                                    .entered_time
                                    .with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M:%S")
                                    .to_string();

                                let status_desc = order
                                    .status_description
                                    .as_deref()
                                    .unwrap_or("");

                                account_section.push_str(&format!(
                                    "  • #{:<12} {:<12} {:<6} {:>6} {:<8}{}{} (entered {})",
                                    order.order_id,
                                    status_label,
                                    instruction_text,
                                    qty_text,
                                    symbol_text,
                                    price_text,
                                    leg_note,
                                    entered_local
                                ));

                                if !status_desc.is_empty() {
                                    account_section.push_str(&format!(" - {}", status_desc));
                                }

                                account_section.push('\n');
                                account_has_open = true;
                            }

                            if !account_has_open {
                                account_section.push_str("  (no open orders)\n");
                            } else {
                                any_open_orders = true;
                            }

                            report.push_str(&account_section);
                        }
                        Err(err) => {
                            report.push_str(&format!(
                                "Account {}:\n  ⚠️ Unable to fetch orders: {}\n",
                                label, err
                            ));
                        }
                    },
                    Err(err) => {
                        report.push_str(&format!(
                            "Account {}:\n  ⚠️ Unable to request orders: {}\n",
                            label, err
                        ));
                    }
                }
            }

            if !any_open_orders {
                report.push_str("  (no open orders found in the past 24 hours)\n");
            }

            report.push_str("\n");
        } else {
            report.push_str("Open Orders:\n  ⚠️ Schwab API client not initialized.\n\n");
        }

        let tracked_symbols: Vec<(Symbol, RealTimeQuote)> = {
            let prices = self.real_time_prices.lock().await;
            let mut seen = HashSet::new();

            self.symbols_config
                .iter()
                .filter_map(|symbol| {
                    if seen.insert(symbol.symbol.clone()) {
                        let quote = prices.get(&symbol.symbol).cloned().unwrap_or_default();
                        Some((symbol.clone(), quote))
                    } else {
                        None
                    }
                })
                .collect()
        };

        if !tracked_symbols.is_empty() {
            report.push_str("Tracked Symbols (regular session):\n");
            report.push_str("Symbol   Signal  Last    %Chg    Volume        Avg 10D Vol\n");
            report.push_str("-------- ------ -------- ------- ------------ -------------\n");
            for (symbol_cfg, quote) in tracked_symbols {
                let signal = match quote.percent_change {
                    Some(change) if change >= symbol_cfg.exit_threshold => "SELL",
                    Some(change) if change <= symbol_cfg.entry_threshold => "BUY",
                    _ => "HOLD",
                };
                let last = quote
                    .last_price
                    .map(|v| format!("${:.2}", v))
                    .unwrap_or_else(|| "-".to_string());
                let pct = quote
                    .percent_change
                    .map(|v| format!("{:+.2}%", v))
                    .unwrap_or_else(|| "-".to_string());
                let volume = quote
                    .total_volume
                    .map(|v| format_int(v.round() as i64))
                    .unwrap_or_else(|| "-".to_string());
                let avg_volume = quote
                    .avg_10_day_volume
                    .map(|v| format_int(v.round() as i64))
                    .unwrap_or_else(|| "-".to_string());

                let short_symbol: String = symbol_cfg.symbol.chars().take(5).collect();

                report.push_str(&format!(
                    "{:<8} {:>6} {:>8} {:>7} {:>12} {:>13}\n",
                    short_symbol, signal, last, pct, volume, avg_volume
                ));
            }
        }

        report
    }

    /// Generate trading signals
    pub async fn generate_signals(&self) -> HashMap<String, SignalInfo> {
        let mut signals = HashMap::new();

        let tracked_symbols: Vec<(Symbol, RealTimeQuote)> = {
            let prices = self.real_time_prices.lock().await;

            self.symbols_config
                .iter()
                .cloned()
                .map(|symbol| {
                    let quote = prices.get(&symbol.symbol).cloned().unwrap_or_default();
                    (symbol, quote)
                })
                .collect()
        };

        for (symbol, quote) in tracked_symbols {
            let (action, amount) = match quote.percent_change {
                Some(change) if change >= symbol.exit_threshold => {
                    ("SELL".to_string(), symbol.exit_amount)
                }
                Some(change) if change <= symbol.entry_threshold => {
                    ("BUY".to_string(), symbol.entry_amount)
                }
                _ => ("HOLD".to_string(), 0.0),
            };

            let key = format!("{}@{}", symbol.symbol, symbol.account_hash);

            let quantity = if amount > 0.0 {
                if let Some(price) = quote.last_price {
                    (amount / price).floor()
                } else {
                    0.0
                }
            } else {
                0.0
            };

            signals.insert(
                key,
                SignalInfo {
                    action,
                    last_price: quote.last_price,
                    quantity,
                },
            );
        }

        signals
    }

    async fn evaluate_order_risks(
        &self,
        api: &Api<SharedTokenChecker>,
        account_hash: &str,
        symbol: &str,
        instruction: Instruction,
        qty: f64,
        limit_price: f64,
    ) -> Result<(), String> {
        let order_notional = qty * limit_price;

        // Risk check 1: ensure sufficient cash for buy orders
        let mut cached_balances: Option<Vec<f64>> = None;
        if matches!(instruction, Instruction::Buy) {
            let balances = Self::get_balance(api, account_hash)
                .await
                .map_err(|e| format!("Unable to fetch balances for BUY risk check: {e}"))?;
            let available_cash = balances.first().copied().unwrap_or(0.0);
            if available_cash + f64::EPSILON < order_notional {
                return Err(format!(
                    "Insufficient cash to buy {} shares of {} for ${:.2}; available ${:.2}",
                    qty, symbol, order_notional, available_cash
                ));
            }
            cached_balances = Some(balances);
        }

        // Risk check 2: position sizing capped at 1% of portfolio value
        let mut portfolio_value = {
            let balances = self.account_balances.lock().await;
            balances.get(account_hash).copied()
        }
        .unwrap_or(0.0);

        if portfolio_value <= 0.0 {
            let balances = match cached_balances {
                Some(ref balances) => balances.clone(),
                None => Self::get_balance(api, account_hash)
                    .await
                    .map_err(|e| format!("Unable to refresh balances for risk check: {e}"))?,
            };
            portfolio_value = balances
                .get(1)
                .copied()
                .or_else(|| balances.last().copied())
                .unwrap_or(0.0);
            if portfolio_value <= 0.0 {
                return Err("Unable to determine portfolio value for risk checks".to_string());
            }
        }

        // Current holdings for symbol (long side only for sizing)
        let held_quantity = {
            let positions = self.positions.lock().await;
            positions
                .get(account_hash)
                .and_then(|m| m.get(symbol))
                .copied()
                .unwrap_or(0.0)
        };

        if matches!(instruction, Instruction::Sell) {
            let available_to_sell = held_quantity.max(0.0);
            if available_to_sell <= f64::EPSILON {
                return Err(format!(
                    "Cannot sell {} when no long position exists (current qty: {:.2})",
                    symbol, held_quantity
                ));
            }
            if qty - available_to_sell > 1e-6 {
                return Err(format!(
                    "Sell quantity {:.2} exceeds held amount {:.2} for {}",
                    qty, available_to_sell, symbol
                ));
            }
        }

        if matches!(instruction, Instruction::Buy) {
            let reference_price = {
                let prices = self.real_time_prices.lock().await;
                prices
                    .get(symbol)
                    .and_then(|q| q.last_price)
                    .unwrap_or(limit_price)
            };

            let existing_notional = (held_quantity.max(0.0)) * reference_price;
            let projected_notional = existing_notional + order_notional;
            let threshold = portfolio_value * 0.01;
            if projected_notional > threshold {
                return Err(format!(
                    "Order would raise {} exposure to ${:.2}, > 1% cap (${:.2}) on portfolio ${:.2}",
                    symbol, projected_notional, threshold, portfolio_value
                ));
            }
        }

        // Risk check 3: ensure limit price is in line with last trade
        if let Some(last_price) = {
            let prices = self.real_time_prices.lock().await;
            prices.get(symbol).and_then(|q| q.last_price)
        } {
            if last_price > 0.0 {
                let deviation = ((limit_price - last_price) / last_price).abs();
                if deviation > 0.20 {
                    return Err(format!(
                        "Limit price ${:.2} deviates {:.1}% from last price ${:.2}",
                        limit_price,
                        deviation * 100.0,
                        last_price
                    ));
                }
            }
        }

        // Optional guardrail: avoid sending ultra-small trades that are likely noise
        if order_notional < 10.0 {
            return Err(format!(
                "Order notional ${:.2} is below the minimum trade size threshold",
                order_notional
            ));
        }

        Ok(())
    }

    /// Estimate Schwab commission for an order (flat fee from env var)
    fn estimate_schwab_commission(&self, _qty: f64, _price: f64) -> f64 {
        std::env::var("SCHWAB_COMMISSION_FLAT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    pub async fn send_limit_order(
        &mut self,
        symbol_key: &str,
        side: &str,
        qty: f64,
        limit_price: f64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (symbol, account_hash) = symbol_key
            .split_once('@')
            .ok_or("Symbol key must be in SYMBOL@ACCOUNT format")?;

        let symbol_cfg = self
            .symbols_config
            .iter()
            .find(|cfg| cfg.account_hash == account_hash && cfg.symbol.eq_ignore_ascii_case(symbol))
            .ok_or("Symbol/account combination not configured")?;

        // Risk check: limit one order per symbol per day
        {
            let mut order_history = self.order_history.lock().await;
            let now = chrono::Utc::now();
            let today = now.date_naive();

            if let Some(last_order_time) = order_history.get(&symbol_cfg.symbol) {
                let last_order_date = last_order_time.date_naive();
                if last_order_date == today {
                    return Err(format!(
                        "Order blocked: only one order per symbol per day allowed. Last order for {} was on {}",
                        symbol_cfg.symbol, last_order_date
                    ).into());
                }
            }

            // Update the order history with current timestamp
            order_history.insert(symbol_cfg.symbol.to_string(), now);
        }

        // Route based on symbol's api field
        if symbol_cfg.api.as_deref() == Some("ibkr") {
            // Route to IBKR
            let ib_client = self
                .ib_client
                .as_ref()
                .ok_or("IB client not initialized")?
                .clone();

            // Build contract and order using rust-ibapi helpers
            let contract = ibapi::contracts::Contract::stock(&symbol_cfg.symbol).build();

            let action = match side.to_ascii_uppercase().as_str() {
                "BUY" | "B" => ibapi::orders::Action::Buy,
                "SELL" | "S" => ibapi::orders::Action::Sell,
                other => {
                    return Err(format!("Unsupported side '{other}'. Expected BUY or SELL.").into())
                }
            };

            if qty <= 0.0 {
                return Err("Quantity must be greater than zero".into());
            }

            if limit_price <= 0.0 {
                return Err("Limit price must be greater than zero".into());
            }

            // Run the same risk checks as Schwab orders before placing the IBKR order
            // Convert action to Schwab Instruction for risk checking
            let instruction = match side.to_ascii_uppercase().as_str() {
                "BUY" | "B" => Instruction::Buy,
                "SELL" | "S" => Instruction::Sell,
                _ => Instruction::Buy, // already validated above
            };

            // Apply the shared risk checks (will use cached balances/positions)
            if let Some(api) = self.api.as_ref() {
                // We call evaluate_order_risks to validate sizing, cash, deviation, etc.
                self.evaluate_order_risks(
                    api,
                    &symbol_cfg.account_hash,
                    &symbol_cfg.symbol,
                    instruction,
                    qty,
                    limit_price,
                )
                .await
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;
            } else {
                // If Schwab API isn't present, still try to perform local checks using cached balances
                // (evaluate_order_risks needs Api for balance fetch on buys). For now, proceed.
            }

            let order = ibapi::orders::order_builder::limit_order(action, qty, limit_price);

            // Get an order id and place the order, subscribing to updates
            let order_id = ib_client.next_order_id();

            match ib_client.place_order(order_id, &contract, &order).await {
                Ok(mut subscription) => {
                    // Spawn background task to monitor order updates for this order
                    let symbol_name = symbol_cfg.symbol.clone();
                    // note: do not rely on the configured account for fills; use the execution's account
                    let account_hash_clone_config = symbol_cfg.account_hash.clone();
                    let positions_map = Arc::clone(&self.positions);
                    let order_history_map = Arc::clone(&self.order_history);

                    tokio::spawn(async move {
                        while let Some(event) = subscription.next().await {
                            match event {
                                Ok(ibapi::orders::PlaceOrder::OrderStatus(status)) => {
                                    eprintln!(
                                        "[IBKR Order {}] status: {} filled: {}/{} avg_fill_price: {:?}",
                                        status.order_id, status.status, status.filled, status.remaining, status.average_fill_price
                                    );

                                    // If order is filled, update order_history timestamp to now
                                    if status.status == "Filled" {
                                        let mut hist = order_history_map.lock().await;
                                        hist.insert(symbol_name.clone(), chrono::Utc::now());
                                    }
                                }
                                Ok(ibapi::orders::PlaceOrder::ExecutionData(exec)) => {
                                    eprintln!(
                                        "[IBKR Execution] {} {} shares @ {} on {}",
                                        exec.contract.symbol, exec.execution.shares, exec.execution.price, exec.execution.exchange
                                    );

                                    // Update cached positions using the execution's account number
                                    let exec_account = if exec.execution.account_number.is_empty() {
                                        account_hash_clone_config.clone()
                                    } else {
                                        exec.execution.account_number.clone()
                                    };
                                    let side = exec.execution.side.to_uppercase();
                                    let shares = exec.execution.shares;

                                    // Persist execution to CSV for auditing
                                    let exec_csv_line = format!("{},{},{},{},{},{},{}\n",
                                        chrono::Utc::now().to_rfc3339(),
                                        exec.execution.execution_id,
                                        exec_account,
                                        exec.contract.symbol.as_str(),
                                        exec.execution.side,
                                        exec.execution.shares,
                                        exec.execution.price
                                    );

                                    if let Ok(mut file) = OpenOptions::new()
                                        .create(true)
                                        .append(true)
                                        .open("output/ibkr_executions.csv")
                                        .await
                                    {
                                        let _ = file.write_all(exec_csv_line.as_bytes()).await;
                                    }

                                    let mut positions_guard = positions_map.lock().await;
                                    let account_entry = positions_guard
                                        .entry(exec_account.clone())
                                        .or_insert_with(HashMap::new);

                                    let current_qty = account_entry
                                        .get(exec.contract.symbol.as_str())
                                        .copied()
                                        .unwrap_or(0.0);

                                    let new_qty = if side == "BUY" || side == "BOT" {
                                        current_qty + shares
                                    } else {
                                        current_qty - shares
                                    };

                                    account_entry.insert(exec.contract.symbol.as_str().to_string(), new_qty);
                                }
                                Ok(ibapi::orders::PlaceOrder::OpenOrder(open_order)) => {
                                    eprintln!(
                                        "[IBKR OpenOrder] {} id {} action {:?} qty {} status {}",
                                        symbol_name, open_order.order.order_id, open_order.order.action, open_order.order.total_quantity, open_order.order_state.status
                                    );
                                }
                                Ok(ibapi::orders::PlaceOrder::CommissionReport(report)) => {
                                    eprintln!("[IBKR Commission] {} {}", report.execution_id, report.commission);

                                    // Persist commission report to CSV
                                    let comm_csv_line = format!("{},{},{},{}\n",
                                        chrono::Utc::now().to_rfc3339(),
                                        report.execution_id,
                                        report.commission,
                                        report.currency
                                    );

                                    if let Ok(mut file) = OpenOptions::new()
                                        .create(true)
                                        .append(true)
                                        .open("output/ibkr_commissions.csv")
                                        .await
                                    {
                                        let _ = file.write_all(comm_csv_line.as_bytes()).await;
                                    }
                                }
                                Ok(ibapi::orders::PlaceOrder::Message(msg)) => {
                                    eprintln!("[IBKR Order Message] {} - {}", msg.code, msg.message);
                                }
                                Err(e) => {
                                    eprintln!("Error in IBKR order subscription: {e}");
                                    break;
                                }
                            }
                        }
                    });

                    eprintln!("✓ IBKR order submitted: {} {} shares @ ${:.2} (order_id {})", side, qty, limit_price, order_id);
                    return Ok(());
                }
                Err(err) => {
                    return Err(format!("Failed to place IBKR order: {}", err).into());
                }
            }
        } else {
            // Route to Schwab (default)
            let api = self
                .api
                .as_ref()
                .ok_or("Schwab API client not initialized")?;

            let instruction = match side.to_ascii_uppercase().as_str() {
                "BUY" | "B" => Instruction::Buy,
                "SELL" | "S" => Instruction::Sell,
                other => {
                    return Err(format!("Unsupported side '{other}'. Expected BUY or SELL.").into())
                }
            };

            // Risk management checks
            if qty <= 0.0 {
                return Err("Quantity must be greater than zero".into());
            }

            if limit_price <= 0.0 {
                return Err("Limit price must be greater than zero".into());
            }

            let rounded_price = (limit_price * 100.0).round() / 100.0;

            // Commission Policy B: block order if estimated commission > 0
            let estimated_commission = self.estimate_schwab_commission(qty, rounded_price);
            if estimated_commission > 0.0 {
                return Err(format!(
                    "Order blocked by commission policy: estimated commission ${:.2} exceeds threshold",
                    estimated_commission
                ).into());
            }

            self.evaluate_order_risks(
                api,
                &symbol_cfg.account_hash,
                &symbol_cfg.symbol,
                instruction,
                qty,
                rounded_price,
            )
            .await
            .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;

            let order = OrderRequest::limit(
                InstrumentRequest::Equity {
                    symbol: symbol_cfg.symbol.clone(),
                },
                instruction,
                qty,
                rounded_price,
            )?;

            api.post_account_order(symbol_cfg.account_hash.clone(), order)
                .await?
                .send()
                .await?;

            eprintln!("✓ Schwab order placed: {}@{} {} shares at ${:.2}", symbol, account_hash, qty, rounded_price);
            Ok(())
        }
    }

    /// Reads and parses the symbols configuration file.
    async fn read_symbols_config(
        file_path: &str,
    ) -> Result<Vec<Symbol>, Box<dyn Error + Send + Sync>> {
        let content = tokio::fs::read_to_string(file_path).await?;
        let symbols: Vec<Symbol> = serde_json::from_str(&content)?;
        Ok(symbols)
    }

}

fn format_int(value: i64) -> String {
    let mut digits = value.abs().to_string();
    let mut parts: Vec<String> = Vec::new();

    while digits.len() > 3 {
        let chunk = digits.split_off(digits.len() - 3);
        parts.push(chunk);
    }

    let mut result = digits;
    while let Some(chunk) = parts.pop() {
        result.push(',');
        result.push_str(&chunk);
    }

    if value < 0 {
        let mut with_sign = String::with_capacity(result.len() + 1);
        with_sign.push('-');
        with_sign.push_str(&result);
        with_sign
    } else {
        result
    }
}

// Implement the telegram_bot::TradingBot trait for our TradingBot
#[async_trait]
impl telegram_bot::TradingBot for TradingBot {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    async fn new() -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        Self::new().await
    }

    async fn execute_strategy(
        &mut self,
        bot_state: Arc<Mutex<BotState>>,
        telegram_bot: Bot,
        chat_id: ChatId,
    ) -> Result<(), Self::Error> {
        crate::schwab_execute_strategy::execute_strategy_internal(
            self,
            bot_state,
            telegram_bot,
            chat_id,
        )
        .await
    }

    async fn get_status(&self) -> String {
        let positions = self.positions.lock().await;
        let balances = self.account_balances.lock().await;
        let prices = self.real_time_prices.lock().await;

        let mut status = String::from("Schwab Trading Bot Status:\n\n");

        if balances.is_empty() {
            status.push_str("Account Balances:\n  (not available)\n\n");
        } else {
            status.push_str("Account Balances:\n");
            for (account_hash, balance) in balances.iter() {
                let label = if account_hash.len() > 8 {
                    format!(
                        "{}…{}",
                        &account_hash[..4],
                        &account_hash[account_hash.len() - 4..]
                    )
                } else {
                    account_hash.clone()
                };
                status.push_str(&format!("  {}: ${:.2}\n", label, balance));
            }
            status.push_str("\n");
        }

        status.push_str("Positions:\n");
        if positions.is_empty() {
            status.push_str("  (no cached positions)\n");
        } else {
            for (account_hash, account_positions) in positions.iter() {
                let label = if account_hash.len() > 8 {
                    format!(
                        "{}…{}",
                        &account_hash[..4],
                        &account_hash[account_hash.len() - 4..]
                    )
                } else {
                    account_hash.clone()
                };

                status.push_str(&format!("  Account {}:\n", label));

                if account_positions.is_empty() {
                    status.push_str("    (no open positions)\n");
                } else {
                    for (symbol, position) in account_positions.iter() {
                        let current_price = prices
                            .get(symbol)
                            .and_then(|quote| quote.last_price)
                            .unwrap_or(0.0);
                        status.push_str(&format!(
                            "    {}: {:.4} units @ ${:.2}\n",
                            symbol, position, current_price
                        ));
                    }
                }
            }
        }

        status.push_str("\nTracked Symbols:\n");
        let mut seen = HashSet::new();
        for symbol in &self.symbols_config {
            if seen.insert(symbol.symbol.clone()) {
                let quote = prices.get(&symbol.symbol);
                let current_price = quote
                    .and_then(|q| q.last_price)
                    .map(|p| format!("${:.2}", p))
                    .unwrap_or_else(|| "-".to_string());
                let percent_change = quote
                    .and_then(|q| q.percent_change)
                    .map(|p| format!("{:+.2}%", p))
                    .unwrap_or_else(|| "-".to_string());
                status.push_str(&format!(
                    "  {}: {} ({})\n",
                    symbol.symbol, current_price, percent_change
                ));
            }
        }

        status
    }

    fn get_config_path(&self) -> &str {
        "/Users/rogerbos/rust_home/schwab_bot/symbols_config.json"
    }

    fn get_interval_seconds(&self) -> u64 {
        60 // in seconds
    }
}
