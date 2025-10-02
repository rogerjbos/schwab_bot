use std::collections::HashMap;
use std::error::Error;
use crate::models::PositionSummary;
use ibapi::orders::ExecutionFilter;

pub async fn fetch_ib_positions(
    owner: &crate::schwab_pos::TradingBot,
    account_id: &str,
    ) -> Result<Vec<PositionSummary>, Box<dyn Error + Send + Sync>> {
    let ib_client = owner.ib_client.as_ref().ok_or("IB client not initialized")?.clone();

    use ibapi::accounts::{types::AccountId, AccountUpdate};

    let account = AccountId::from(account_id);
    let mut subscription = ib_client.account_updates(&account).await?;

    let mut positions: HashMap<String, PositionSummary> = HashMap::new();

    while let Some(update_result) = subscription.next().await {
        match update_result {
            Ok(AccountUpdate::PortfolioValue(value)) => {
                if value.position.abs() < f64::EPSILON {
                    continue;
                }

                let contract = value.contract;
                let symbol = contract.symbol.to_string();
                let description = if contract.description.is_empty() {
                    None
                } else {
                    Some(contract.description.clone())
                };
                let asset_type = Some(format!("{}", contract.security_type));

                let quantity = value.position;
                let is_short = quantity < 0.0;
                let average_price = if value.average_cost == 0.0 {
                    None
                } else {
                    Some(value.average_cost)
                };
                let market_value = if value.market_value == 0.0 {
                    None
                } else {
                    Some(value.market_value)
                };
                let cost_basis = average_price.map(|avg| avg * quantity.abs());
                let profit_loss = if value.unrealized_pnl == 0.0 {
                    None
                } else {
                    Some(value.unrealized_pnl)
                };

                positions.insert(
                    symbol.clone(),
                        PositionSummary {
                            symbol,
                            description,
                            asset_type,
                            quantity,
                            is_short,
                            average_price,
                            market_value,
                            cost_basis,
                            profit_loss,
                        },
                );
            }
            Ok(AccountUpdate::End) => {
                subscription.cancel().await;
                break;
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("Error receiving IB account update: {err}");
                break;
            }
        }
    }

    if positions.is_empty() {
        subscription.cancel().await;
    }

    Ok(positions.into_values().collect())
}

pub async fn fetch_ib_balance(
    owner: &crate::schwab_pos::TradingBot,
    account_id: &str,
) -> Result<f64, Box<dyn Error + Send + Sync>> {
    let ib_client = owner.ib_client.as_ref().ok_or("IB client not initialized")?.clone();

    use ibapi::accounts::{types::AccountId, AccountUpdate};

    let account = AccountId::from(account_id);
    let mut subscription = ib_client.account_updates(&account).await?;

    let mut net_liquidation: Option<f64> = None;
    let mut total_cash: Option<f64> = None;

    while let Some(update_result) = subscription.next().await {
        match update_result {
            Ok(AccountUpdate::AccountValue(value)) => {
                let tag = value.key.to_string();
                let amount = value.value.parse::<f64>().unwrap_or(0.0);

                match tag.as_str() {
                    "NetLiquidation" => net_liquidation = Some(amount),
                    "TotalCashValue" => total_cash = Some(amount),
                    _ => {}
                }
            }
            Ok(AccountUpdate::End) => {
                subscription.cancel().await;
                break;
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("Error receiving IB account update for balance: {err}");
                break;
            }
        }
    }

    if net_liquidation.is_none() && total_cash.is_none() {
        subscription.cancel().await;
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("No account balance data received for {}", account_id),
        )
        .into());
    }

    let balance_value = net_liquidation.or(total_cash).unwrap_or(0.0);
    Ok(balance_value)
}

pub async fn populate_order_history_from_ibkr(
    client: &ibapi::Client,
) -> Result<HashMap<String, chrono::DateTime<chrono::Utc>>, Box<dyn Error + Send + Sync>> {
    let mut result_map: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();

    // 1) Fetch today's executions
    let filter = ExecutionFilter::default();
    match client.executions(filter).await {
        Ok(mut sub) => {
            while let Some(item) = sub.next().await {
                match item {
                    Ok(ibapi::orders::Executions::ExecutionData(exec)) => {
                        let sym = exec.contract.symbol.as_str().to_string();
                        // parse exec.execution.time if possible; otherwise use now
                        let when = chrono::Utc::now();
                        result_map.insert(sym, when);
                    }
                    Ok(ibapi::orders::Executions::CommissionReport(_)) => {}
                    Err(e) => {
                        eprintln!("Error reading execution stream: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to request IBKR executions: {}", e);
        }
    }

    // 2) Fetch current open orders placed by this API client
    match client.open_orders().await {
        Ok(mut sub) => {
            while let Some(item) = sub.next().await {
                match item {
                    Ok(ibapi::orders::Orders::OrderData(order_data)) => {
                        // OrderData contains contract directly
                        let sym = order_data.contract.symbol.as_str().to_string();
                        result_map.insert(sym, chrono::Utc::now());
                    }
                    Ok(ibapi::orders::Orders::OrderStatus(_)) => {}
                    Ok(ibapi::orders::Orders::Notice(_)) => {}
                    Err(e) => {
                        eprintln!("Error reading open_orders stream: {}", e);
                        break;
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to request IBKR open orders: {}", e);
        }
    }

    // 3) Completed orders may also indicate fills earlier in the day
    match client.completed_orders(false).await {
        Ok(mut sub) => {
            while let Some(item) = sub.next().await {
                match item {
                    Ok(ibapi::orders::Orders::OrderData(order_data)) => {
                        // Try to extract symbol from order_data.order or contract
                        let sym = order_data.contract.symbol.as_str().to_string();
                        result_map.insert(sym, chrono::Utc::now());
                    }
                    Ok(ibapi::orders::Orders::OrderStatus(_)) => {}
                    Ok(ibapi::orders::Orders::Notice(_)) => {}
                    Err(e) => {
                        eprintln!("Error reading completed_orders stream: {}", e);
                        break;
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to request IBKR completed orders: {}", e);
        }
    }

    Ok(result_map)
}
