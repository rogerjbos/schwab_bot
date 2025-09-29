use async_trait::async_trait;
use chrono;
use serde::{Deserialize, Serialize};
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

// Schwab API imports
use reqwest::Client;
use schwab_api::api::Api;
use schwab_api::error::Error as SchwabError;
use schwab_api::model::market_data::quote_response::QuoteResponse;
use schwab_api::model::{Instruction, InstrumentRequest, OrderRequest};
use schwab_api::token::{TokenChecker, Tokener};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub symbol: String,
    pub account_hash: String,
    pub entry_amount: f64,
    pub exit_amount: f64,
    pub entry_threshold: f64,
    pub exit_threshold: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolConfig {
    pub symbol: String,
    pub account_hash: String,
    pub entry_amount: f64,
    pub exit_amount: f64,
    pub entry_threshold: f64,
    pub exit_threshold: f64,
}

#[derive(Debug)]
struct PositionSummary {
    symbol: String,
    description: Option<String>,
    asset_type: Option<String>,
    quantity: f64,
    is_short: bool,
    average_price: Option<f64>,
    market_value: Option<f64>,
    cost_basis: Option<f64>,
    profit_loss: Option<f64>,
}

#[derive(Debug, Clone, Default)]
struct RealTimeQuote {
    last_price: Option<f64>,
    total_volume: Option<f64>,
    percent_change: Option<f64>,
    avg_10_day_volume: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct SignalInfo {
    pub action: String,
    pub last_price: Option<f64>,
    pub quantity: f64,
}

/// Main trading bot implementation with Schwab API integration.
pub struct TradingBot {
    pub api: Option<Api<SharedTokenChecker>>,
    pub tokener: SharedTokenChecker,
    pub account_hashes: Vec<String>,
    pub positions: Arc<Mutex<HashMap<String, HashMap<String, f64>>>>,
    real_time_prices: Arc<Mutex<HashMap<String, RealTimeQuote>>>,
    pub symbols_config: Vec<Symbol>,
    pub account_balances: Arc<Mutex<HashMap<String, f64>>>,
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

    async fn access_token(&self) -> Result<String, SchwabError> {
        Tokener::get_access_token(&*self.inner).await
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

        for symbol_config in raw_symbols {
            account_hash_set.insert(symbol_config.account_hash.clone());
            symbols.push(Symbol {
                symbol: symbol_config.symbol,
                account_hash: symbol_config.account_hash,
                entry_amount: symbol_config.entry_amount,
                exit_amount: symbol_config.exit_amount,
                entry_threshold: symbol_config.entry_threshold,
                exit_threshold: symbol_config.exit_threshold,
            });
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

        Ok(Self {
            api: Some(api),
            tokener: shared_tokener,
            account_hashes,
            positions: Arc::new(Mutex::new(HashMap::new())),
            real_time_prices: Arc::new(Mutex::new(HashMap::new())),
            symbols_config: symbols,
            account_balances: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn fetch_positions(
        tokener: &SharedTokenChecker,
        account_hash: &str,
    ) -> Result<Vec<PositionSummary>, Box<dyn std::error::Error + Send + Sync>> {
        let access_token = tokener.access_token().await?;
        let client = Client::new();
        let url =
            format!("https://api.schwabapi.com/trader/v1/accounts/{account_hash}?fields=positions");

        let response = client
            .get(url)
            .bearer_auth(access_token)
            .header("Accept", "application/json")
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(format!("Failed to fetch positions: {} - {}", status, body).into());
        }

        let payload: Value = serde_json::from_str(&body)?;
        let mut pos = Vec::new();

        if let Some(account_obj) = payload.get("securitiesAccount") {
            if let Some(entries) = account_obj.get("positions").and_then(|v| v.as_array()) {
                for entry in entries {
                    let instrument = entry.get("instrument").and_then(|v| v.as_object());

                    let symbol = instrument
                        .and_then(|inst| inst.get("symbol"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("UNKNOWN")
                        .to_string();

                    let description = instrument
                        .and_then(|inst| inst.get("description"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let asset_type = instrument
                        .and_then(|inst| inst.get("assetType"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let long_qty = entry
                        .get("longQuantity")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let short_qty = entry
                        .get("shortQuantity")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);

                    let is_short = short_qty > 0.0 && short_qty >= long_qty;
                    let quantity = if is_short { -short_qty } else { long_qty };

                    let average_price = entry.get("averagePrice").and_then(|v| v.as_f64());
                    let market_value = entry.get("marketValue").and_then(|v| v.as_f64());
                    let cost_basis = entry.get("currentDayCost").and_then(|v| v.as_f64());
                    let profit_loss = entry.get("longOpenProfitLoss").and_then(|v| v.as_f64());

                    pos.push(PositionSummary {
                        symbol,
                        description,
                        asset_type,
                        quantity,
                        is_short,
                        average_price,
                        market_value,
                        cost_basis,
                        profit_loss,
                    });
                }
            }
        }

        Ok(pos)
    }

    async fn get_balance(
        api: &Api<SharedTokenChecker>,
        account_hash: &str,
    ) -> Result<Vec<f64>, Box<dyn std::error::Error + Send + Sync>> {
        let account = api
            .get_account(account_hash.to_string())
            .await?
            .send()
            .await?;

        // Access the cash balance properly
        match &account.securities_account {
            schwab_api::model::trader::accounts::SecuritiesAccount::Cash(cash_account) => {
                if let Some(current_balances) = &cash_account.current_balances {
                    return Ok(vec![
                        current_balances.cash_available_for_trading,
                        current_balances.liquidation_value.unwrap_or(0.0),
                    ]);
                }
            }
            schwab_api::model::trader::accounts::SecuritiesAccount::Margin(margin_account) => {
                if let Some(current_balances) = &margin_account.current_balances {
                    return Ok(vec![
                        current_balances.available_funds,
                        current_balances.buying_power,
                        current_balances.equity,
                    ]);
                }
            }
        }

        // Return empty vec if no balances found
        Ok(vec![])
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
            match Self::get_balance(api, account_hash).await {
                Ok(values) if !values.is_empty() => {
                    let primary_value = match values.as_slice() {
                        [_, liquidation] => *liquidation,
                        [_, _, equity] => *equity,
                        slice => {
                            eprintln!(
                                "⚠️ Unexpected balance format ({} values) when updating account info for {}.",
                                slice.len(),
                                account_hash
                            );
                            *slice.last().unwrap_or(&0.0)
                        }
                    };

                    aggregated_balances.insert(account_hash.clone(), primary_value);
                    any_success = true;
                }
                Ok(_) => {
                    eprintln!(
                        "⚠️ Balance payload was empty when updating account info for {}.",
                        account_hash
                    );
                }
                Err(err) => {
                    eprintln!("⚠️ Unable to fetch balances for {}: {}", account_hash, err);
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

    /// Update positions from Schwab API
    pub async fn update_positions(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut aggregated_positions: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut derived_quotes: HashMap<String, RealTimeQuote> = HashMap::new();
        let mut any_success = false;

        for account_hash in &self.account_hashes {
            match Self::fetch_positions(&self.tokener, account_hash).await {
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
                    eprintln!("⚠️ Unable to fetch positions for {}: {}", account_hash, err);
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
    pub async fn generate_status_report(&self) -> String {
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

            report.push_str(&format!("Positions for account {}:\n", label));

            match Self::fetch_positions(&self.tokener, account_hash).await {
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
                    "Order would raise {} exposure to ${:.2}, breaching 1% cap (${:.2}) on portfolio ${:.2}",
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

    pub async fn send_limit_order(
        &self,
        symbol_key: &str,
        side: &str,
        qty: f64,
        limit_price: f64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let api = self
            .api
            .as_ref()
            .ok_or("Schwab API client not initialized")?;

        let (symbol, account_hash) = symbol_key
            .split_once('@')
            .ok_or("Symbol key must be in SYMBOL@ACCOUNT format")?;

        let symbol_cfg = self
            .symbols_config
            .iter()
            .find(|cfg| cfg.account_hash == account_hash && cfg.symbol.eq_ignore_ascii_case(symbol))
            .ok_or("Symbol/account combination not configured")?;

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

        Ok(())
    }

    /// Reads and parses the symbols configuration file.
    async fn read_symbols_config(
        file_path: &str,
    ) -> Result<Vec<SymbolConfig>, Box<dyn Error + Send + Sync>> {
        let content = tokio::fs::read_to_string(file_path).await?;
        let symbols_config: Vec<SymbolConfig> = serde_json::from_str(&content)?;
        Ok(symbols_config)
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
