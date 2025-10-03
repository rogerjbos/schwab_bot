use std::sync::Arc;

use telegram_bot::{send_telegram_notification, BotState, NotificationLevel};
use teloxide::{types::ChatId, Bot};
use tokio::sync::Mutex;

use crate::trading_bot::TradingBot;

/// Execute the trading strategy for the Schwab bot
pub async fn execute_strategy_internal(
    bot: &mut TradingBot,
    _bot_state: Arc<Mutex<BotState>>,
    telegram_bot: Bot,
    chat_id: ChatId,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Update account information
    bot.update_account_info().await?;

    // Update positions
    bot.update_positions().await?;

    // Update prices for all symbols
    bot.update_prices().await?;

    // Generate trading signals
    let signals = bot.generate_signals().await;

    // Execute trades based on signals
    for (symbol_key, signal) in signals {
        let last_price_display = signal
            .last_price
            .map(|price| format!("${:.2}", price))
            .unwrap_or_else(|| "N/A".to_string());

        let short_key: String = symbol_key.chars().take(10).collect();
        println!(
            "Signal for {}: {} (last_price: {}, quantity: {:.2})",
            short_key, signal.action, last_price_display, signal.quantity
        );
        let action = signal.action.to_ascii_uppercase();
        if (action == "BUY" || action == "SELL") && signal.quantity > 0.0 {
            match signal.last_price {
                Some(limit_price) => {
                    if let Err(err) = bot
                        .send_limit_order(&symbol_key, &action, signal.quantity, limit_price)
                        .await
                    {
                        eprintln!("⚠️ Failed to place order for {}: {}", short_key, err);
                    }
                }
                None => {
                    eprintln!(
                        "⚠️ Skipping order for {}: no last price available to use as limit",
                        short_key
                    );
                }
            }
        }
    }

    // Generate and send status report
    let status = bot.generate_status_report().await;

    send_telegram_notification(
        &telegram_bot,
        chat_id,
        NotificationLevel::Important,
        NotificationLevel::Important,
        status,
    )
    .await
    .unwrap_or_else(|e| eprintln!("Failed to send status: {}", e));

    Ok(())
}
