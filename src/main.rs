use std::{error::Error, sync::Arc};

use dotenv::dotenv;
use log;

mod schwab_execute_strategy;
mod schwab_pos;

use telegram_bot::{BotState, Command, TelegramBotHandler};
use teloxide::{prelude::*, types::ChatId, update_listeners, utils::command::BotCommands};
use tokio::sync::Mutex;

use crate::schwab_pos::TradingBot;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // Load .env if present
    dotenv().ok();

    // Read Schwab API credentials
    let schwab_key =
        std::env::var("SCHWAB_API_KEY").expect("SCHWAB_KEY must be set as environment variable");
    let _schwab_secret = std::env::var("SCHWAB_API_SECRET")
        .expect("SCHWAB_SECRET must be set as environment variable");
    log::info!("Using Schwab API key: {}", schwab_key);

    // Read Telegram bot token
    let bot_token = std::env::var("SCHWAB_TELOXIDE_TOKEN")
        .expect("SCHWAB_TELOXIDE_TOKEN must be set as environment variable");
    log::info!("Using Schwab Teloxide Token: {}", bot_token);
    let tg_bot = Bot::new(bot_token);

    // Read chat ID
    let schwab_bot_chat_id: i64 = std::env::var("SCHWAB_BOT_CHAT_ID")
        .expect("SCHWAB_BOT_CHAT_ID must be set as environment variable")
        .parse()
        .expect("SCHWAB_BOT_CHAT_ID must be a valid number");
    let chat_id = ChatId(schwab_bot_chat_id);
    tg_bot
        .send_message(chat_id, "Schwab bot application started")
        .await?;

    // Initialize bot state
    let schwab_bot_state = Arc::new(Mutex::new(BotState {
        is_running: true,
        ..Default::default()
    }));

    // Create Telegram handler and request channel
    let (telegram_handler, request_rx) = TelegramBotHandler::new();

    // Start the trading bot logic (no restart loop)
    let bot_state_clone = Arc::clone(&schwab_bot_state);
    let tg_bot_clone = tg_bot.clone();
    TelegramBotHandler::init_and_run_bot::<TradingBot>(
        bot_state_clone,
        tg_bot_clone,
        chat_id,
        request_rx,
    )
    .await?;

    let cloned_tg_bot = tg_bot.clone();
    let cloned_handler = Arc::new(Mutex::new(telegram_handler));

    teloxide::repl_with_listener(
        tg_bot.clone(),
        move |msg: Message| {
            let schwab_bot_state = Arc::clone(&schwab_bot_state);
            let tg_bot = cloned_tg_bot.clone();
            let handler = Arc::clone(&cloned_handler);
            async move {
                if let Some(cmd) = Command::parse(
                    &msg.text().unwrap_or_default(),
                    tg_bot
                        .get_me()
                        .await?
                        .user
                        .username
                        .as_deref()
                        .unwrap_or(""),
                )
                .ok()
                {
                    let mut handler_guard = handler.lock().await;
                    match handler_guard
                        .handle_command(tg_bot, msg, cmd, schwab_bot_state)
                        .await
                    {
                        Ok(_) => Ok(()),
                        Err(e) => {
                            eprintln!("Error in command handler: {}", e);
                            Ok(())
                        }
                    }
                } else {
                    Ok(())
                }
            }
        },
        update_listeners::polling_default(tg_bot).await,
    )
    .await;

    Ok(())
}
