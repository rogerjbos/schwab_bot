use serde::{Deserialize, Serialize};

/// Shared data types used across the bot (positions, quotes etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSummary {
    pub symbol: String,
    pub description: Option<String>,
    pub asset_type: Option<String>,
    pub quantity: f64,
    pub is_short: bool,
    pub average_price: Option<f64>,
    pub market_value: Option<f64>,
    pub cost_basis: Option<f64>,
    pub profit_loss: Option<f64>,
}

/// Lightweight real-time quote summary used by the bot UI and risk checks
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RealTimeQuote {
    pub last_price: Option<f64>,
    pub total_volume: Option<f64>,
    pub percent_change: Option<f64>,
    pub avg_10_day_volume: Option<f64>,
}

/// Signal produced by the strategy for a symbol
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalInfo {
    pub action: String,
    pub last_price: Option<f64>,
    pub quantity: f64,
}

/// Symbol configuration loaded from symbols_config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub symbol: String,
    pub api: Option<String>,
    pub account_hash: String,
    pub entry_amount: f64,
    pub exit_amount: f64,
    pub entry_threshold: f64,
    pub exit_threshold: f64,
}
