use crate::schwab_pos::SharedTokenChecker;
use crate::models::{PositionSummary, Symbol};
use schwab_api::token::Tokener;
use serde_json::Value;
use std::collections::HashMap;
use std::error::Error;

pub async fn fetch_positions_schwab(
    tokener: &SharedTokenChecker,
    account_hash: &str,
) -> Result<Vec<PositionSummary>, Box<dyn Error + Send + Sync>> {
    let access_token = tokener.get_access_token().await?;
    let client = reqwest::Client::new();
    let url = format!("https://api.schwabapi.com/trader/v1/accounts/{account_hash}?fields=positions");

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

pub async fn get_balance_schwab(
    api: &schwab_api::api::Api<SharedTokenChecker>,
    account_hash: &str,
) -> Result<Vec<f64>, Box<dyn Error + Send + Sync>> {
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

pub async fn populate_order_history_from_schwab(
    tokener: &SharedTokenChecker,
    api: &schwab_api::api::Api<SharedTokenChecker>,
    symbols: &Vec<Symbol>,
) -> Result<HashMap<String, chrono::DateTime<chrono::Utc>>, Box<dyn Error + Send + Sync>> {
    let mut map: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();

    // Determine unique accounts for symbols that use Schwab (skip symbols configured for ibkr)
    let mut accounts: Vec<String> = symbols
        .iter()
        .filter(|s| s.api.as_deref() != Some("ibkr"))
        .map(|s| s.account_hash.clone())
        .collect();
    accounts.sort();
    accounts.dedup();

    let now = chrono::Utc::now();
    let start = now - chrono::Duration::hours(24);

    for acct in accounts {
        // First attempt: use typed Schwab client
        match api.get_account_orders(acct.clone(), start, now).await {
            Ok(req) => match req.send().await {
                Ok(orders) => {
                    for order in orders {
                        // Extract symbol(s) from orderLegCollection
                        let order_json = serde_json::to_value(&order).unwrap_or(Value::Null);
                        if let Some(legs) = order_json.get("orderLegCollection").and_then(|v| v.as_array()) {
                            for leg in legs {
                                if let Some(sym) = leg.get("instrument").and_then(|i| i.get("symbol")).and_then(|s| s.as_str()) {
                                    map.insert(sym.to_string(), chrono::Utc::now());
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    // The typed client failed (could be decoding errors). Fall back to raw HTTP.
                    eprintln!("Typed client failed to fetch Schwab orders for {}: {} — falling back to raw HTTP", acct, e);
                    if let Ok(fallback_map) = fetch_schwab_orders_raw(tokener, &acct).await {
                        map.extend(fallback_map.into_iter());
                    }
                }
            },
            Err(e) => {
                // Possibly invalid account number (Schwab client may surface ServiceError). Try fallback.
                eprintln!("Typed request error for Schwab account {}: {} — falling back to raw HTTP", acct, e);
                if let Ok(fallback_map) = fetch_schwab_orders_raw(tokener, &acct).await {
                    map.extend(fallback_map.into_iter());
                }
            }
        }
    }

    Ok(map)
}

/// Raw HTTP fallback to query Schwab orders for an account using the bearer token from tokener.
async fn fetch_schwab_orders_raw(
    tokener: &SharedTokenChecker,
    account_hash: &str,
) -> Result<HashMap<String, chrono::DateTime<chrono::Utc>>, Box<dyn Error + Send + Sync>> {
    let mut map = HashMap::new();
    let token = tokener.get_access_token().await?;
    let client = reqwest::Client::new();
    let now = chrono::Utc::now();
    let start = now - chrono::Duration::hours(24);

    // Schwab undocumented endpoint for orders listing (mirror of client call). Use query params for timeframe.
    let url = format!("https://api.schwabapi.com/trader/v1/accounts/{}/orders?fromDate={}&toDate={}",
        account_hash,
        start.to_rfc3339(),
        now.to_rfc3339()
    );

    let resp = client
        .get(&url)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!("Schwab raw orders request failed: {}", resp.status()).into());
    }

    let body = resp.text().await?;
    // Parse as JSON and defensively extract symbol strings
    let json: serde_json::Value = serde_json::from_str(&body)?;
    if let Some(orders) = json.as_array() {
        for order in orders {
            if let Some(legs) = order.get("orderLegCollection").and_then(|v| v.as_array()) {
                for leg in legs {
                    if let Some(sym) = leg.get("instrument").and_then(|i| i.get("symbol")).and_then(|s| s.as_str()) {
                        map.insert(sym.to_string(), chrono::Utc::now());
                    }
                }
            }
        }
    }

    Ok(map)
}
