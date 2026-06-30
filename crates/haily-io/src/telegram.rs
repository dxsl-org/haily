use crate::{Adapter, Notification, Request, RequestSender, ResponseChunk};
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};
use uuid::Uuid;

/// Telegram bot adapter. Requires `TELOXIDE_TOKEN` env var at runtime.
///
/// Routing: chat_id (Telegram i64) ↔ session_id (Haily UUID).
/// Response streaming: Text chunks are buffered; the full message is sent on Complete.
pub struct TelegramAdapter {
    bot: Bot,
    chat_to_session: Arc<DashMap<i64, Uuid>>,
    session_to_chat: Arc<DashMap<Uuid, i64>>,
    /// Accumulates streamed text per session; sent as one Telegram message on Complete.
    text_buffer: Arc<DashMap<Uuid, String>>,
}

impl TelegramAdapter {
    /// Create from an explicit token. Pass `None` to read from `TELOXIDE_TOKEN`.
    pub fn new(token: Option<String>) -> Self {
        let bot = match token {
            Some(t) => Bot::new(t),
            None => Bot::from_env(),
        };
        Self {
            bot,
            chat_to_session: Arc::new(DashMap::new()),
            session_to_chat: Arc::new(DashMap::new()),
            text_buffer: Arc::new(DashMap::new()),
        }
    }
}

#[async_trait]
impl Adapter for TelegramAdapter {
    /// Starts the Telegram polling loop in a background task.
    async fn start(&self, tx: RequestSender) -> Result<()> {
        let bot = self.bot.clone();
        let chat_to_session = Arc::clone(&self.chat_to_session);
        let session_to_chat = Arc::clone(&self.session_to_chat);
        let tx = Arc::new(tx);

        tokio::spawn(async move {
            let handler = Update::filter_message().endpoint({
                let tx = Arc::clone(&tx);
                let c2s = Arc::clone(&chat_to_session);
                let s2c = Arc::clone(&session_to_chat);

                move |msg: Message| {
                    let tx = Arc::clone(&tx);
                    let c2s = Arc::clone(&c2s);
                    let s2c = Arc::clone(&s2c);
                    async move {
                        let Some(text) = msg.text() else {
                            return respond(());
                        };
                        let chat_id = msg.chat.id.0;
                        let user_ref = msg
                            .from()
                            .map(|u| u.username.clone().unwrap_or_else(|| u.id.to_string()));

                        // Stable session per chat_id
                        let session_id = *c2s
                            .entry(chat_id)
                            .or_insert_with(Uuid::new_v4);
                        s2c.insert(session_id, chat_id);

                        let req = Request {
                            session_id,
                            adapter_id: "telegram".to_string(),
                            message: text.to_string(),
                            user_ref,
                        };

                        if tx.send(req).await.is_err() {
                            tracing::warn!("telegram: orchestrator channel closed");
                        }

                        respond(())
                    }
                }
            });

            Dispatcher::builder(bot, handler)
                .enable_ctrlc_handler()
                .build()
                .dispatch()
                .await;
        });

        Ok(())
    }

    async fn deliver(&self, session_id: Uuid, chunk: ResponseChunk) -> Result<()> {
        match chunk {
            ResponseChunk::Text(text) => {
                self.text_buffer
                    .entry(session_id)
                    .or_default()
                    .push_str(&text);
            }
            ResponseChunk::Complete => {
                if let Some((_, text)) = self.text_buffer.remove(&session_id) {
                    if let Some(chat_id) = self.session_to_chat.get(&session_id) {
                        let trimmed = text.trim().to_string();
                        if !trimmed.is_empty() {
                            self.bot
                                .send_message(ChatId(*chat_id), trimmed)
                                .parse_mode(ParseMode::Html)
                                .await?;
                        }
                    }
                }
            }
            ResponseChunk::ToolApprovalRequest { tool, args, .. } => {
                if let Some(chat_id) = self.session_to_chat.get(&session_id) {
                    let msg = format!("⚙️ <b>Tool approval needed</b>\n<code>{tool}</code>\n{args}");
                    self.bot
                        .send_message(ChatId(*chat_id), msg)
                        .parse_mode(ParseMode::Html)
                        .await?;
                }
            }
            ResponseChunk::ToolResult { name, ok } => {
                // Silent — tool results are embedded in the next text response
                let _ = (name, ok);
            }
        }
        Ok(())
    }

    async fn notify(&self, msg: Notification) -> Result<()> {
        // WorkItemsChanged is a terminal/panel concern — message channels don't have
        // a persistent status area to update, so we skip it here.
        if matches!(msg, Notification::WorkItemsChanged(_)) {
            return Ok(());
        }
        let text = match msg {
            Notification::MorningBrief(brief) => {
                format!("🌅 <b>Morning Brief</b>\n{brief}")
            }
            Notification::Alert { title, body, urgent } => {
                let icon = if urgent { "🔴" } else { "📢" };
                format!("{icon} <b>{title}</b>\n{body}")
            }
            Notification::ReminderFired { title, .. } => {
                format!("⏰ <b>Reminder</b>: {title}")
            }
            Notification::WorkItemsChanged(_) => unreachable!(),
        };

        // Broadcast to all known chats
        for entry in self.session_to_chat.iter() {
            let chat_id = *entry.value();
            if let Err(e) = self
                .bot
                .send_message(ChatId(chat_id), &text)
                .parse_mode(ParseMode::Html)
                .await
            {
                tracing::warn!("telegram notify to chat {chat_id} failed: {e:#}");
            }
        }
        Ok(())
    }

    fn id(&self) -> &str {
        "telegram"
    }
}
