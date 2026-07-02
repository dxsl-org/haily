use crate::{Adapter, Notification, Request, RequestSender, ResponseChunk};
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};
use uuid::Uuid;

/// Escape the three characters Telegram's HTML `parse_mode` treats as markup so
/// untrusted text (tool args, LLM output, DB-stored titles/bodies) cannot break out
/// of the intended tags or inject new ones. Telegram's HTML subset has no attribute
/// surface, so `&`/`<`/`>` are sufficient — quotes need no escaping here.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

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
                            // Buffered LLM output is untrusted — it may contain
                            // characters that would otherwise be read as HTML markup
                            // (or a breakout of the message context) by Telegram.
                            self.bot
                                .send_message(ChatId(*chat_id), escape_html(&trimmed))
                                .parse_mode(ParseMode::Html)
                                .await?;
                        }
                    }
                }
            }
            ResponseChunk::ToolApprovalRequest { tool, args, .. } => {
                if let Some(chat_id) = self.session_to_chat.get(&session_id) {
                    let msg = format!(
                        "⚙️ <b>Tool approval needed</b>\n<code>{}</code>\n{}",
                        escape_html(&tool),
                        escape_html(&args)
                    );
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
                format!("🌅 <b>Morning Brief</b>\n{}", escape_html(&brief))
            }
            Notification::Alert { title, body, urgent } => {
                let icon = if urgent { "🔴" } else { "📢" };
                format!("{icon} <b>{}</b>\n{}", escape_html(&title), escape_html(&body))
            }
            Notification::ReminderFired { title, .. } => {
                format!("⏰ <b>Reminder</b>: {}", escape_html(&title))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_neutralizes_closing_bold_tag() {
        let payload = "</b>pwned<b>";
        let out = escape_html(payload);
        assert!(!out.contains("</b>"));
        assert!(!out.contains("<b>"));
        assert_eq!(out, "&lt;/b&gt;pwned&lt;b&gt;");
    }

    #[test]
    fn escape_html_neutralizes_code_and_bold_breakout() {
        // Simulates a reminder title crafted to break out of the surrounding <code>/<b> tags.
        let payload = "</code><b>x</b>";
        let out = escape_html(payload);
        assert!(!out.contains("</code>"));
        assert!(!out.contains("<b>"));
        assert!(!out.contains("</b>"));
    }

    #[test]
    fn escape_html_escapes_ampersand() {
        assert_eq!(escape_html("Tom & Jerry"), "Tom &amp; Jerry");
    }

    #[test]
    fn escape_html_leaves_plain_text_unchanged() {
        assert_eq!(escape_html("Nhắc nhở lúc 9h sáng"), "Nhắc nhở lúc 9h sáng");
    }
}
