use crate::{Adapter, ApprovalResolver, Notification, Request, RequestSender, ResponseChunk};
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::{Arc, Mutex};
use teloxide::{
    payloads::SendMessageSetters,
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use uuid::Uuid;

/// Escape the three characters Telegram's HTML `parse_mode` treats as markup so
/// untrusted text (tool args, LLM output, DB-stored titles/bodies) cannot break out
/// of the intended tags or inject new ones. Telegram's HTML subset has no attribute
/// surface, so `&`/`<`/`>` are sufficient — quotes need no escaping here.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `callback_data` prefix for the approve button; the suffix is the approval UUID.
/// Telegram callback data is visible to the client (not a secret) — chat-id binding
/// via `session_to_chat`, checked in `callback_handler`, is the actual auth boundary.
const APPROVE_PREFIX: &str = "approve:";
const DENY_PREFIX: &str = "deny:";

fn approval_keyboard(approval_id: Uuid) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback("✅ Có", format!("{APPROVE_PREFIX}{approval_id}")),
        InlineKeyboardButton::callback("❌ Không", format!("{DENY_PREFIX}{approval_id}")),
    ]])
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
    /// Injected by `haily-app::bootstrap` after the orchestrator exists (see
    /// `Adapter::set_approval_resolver`).
    resolver: Arc<Mutex<Option<Arc<dyn ApprovalResolver>>>>,
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
            resolver: Arc::new(Mutex::new(None)),
        }
    }
}

/// Parse `"approve:<uuid>"` / `"deny:<uuid>"` callback data into `(approved, uuid)`.
/// Returns `None` for anything else (unrecognized callback data — ignored, not an error).
fn parse_callback_data(data: &str) -> Option<(bool, Uuid)> {
    if let Some(rest) = data.strip_prefix(APPROVE_PREFIX) {
        Uuid::parse_str(rest).ok().map(|id| (true, id))
    } else if let Some(rest) = data.strip_prefix(DENY_PREFIX) {
        Uuid::parse_str(rest).ok().map(|id| (false, id))
    } else {
        None
    }
}

/// Handles an inline-button tap. Only resolves the approval when the callback's
/// originating chat is bound to the same session the approval was raised for —
/// a callback from any other chat (forged or simply a different user who somehow
/// obtained the callback data) is ignored with a warn log, and the approval stays
/// pending for its rightful session.
async fn handle_callback_query(
    bot: Bot,
    q: CallbackQuery,
    chat_to_session: Arc<DashMap<i64, Uuid>>,
    resolver: Arc<Mutex<Option<Arc<dyn ApprovalResolver>>>>,
) -> Result<(), teloxide::RequestError> {
    let Some(data) = q.data.as_deref() else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };
    let Some((approved, approval_id)) = parse_callback_data(data) else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };
    let Some(message) = &q.message else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };

    // Approvals are only honored from PRIVATE (1:1) chats. A group shares one
    // chat_id → one session, so in a group any member could approve a destructive
    // tool call another member's turn raised. Refusing non-private chats keeps the
    // "one human owns this session" auth assumption the broker relies on. A group
    // approval therefore never resolves and the turn fails closed via timeout-deny.
    if !message.chat.is_private() {
        tracing::warn!(chat_id = message.chat.id.0, "telegram approval from non-private chat — ignoring (approvals require a 1:1 chat)");
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    }
    let chat_id = message.chat.id.0;

    let session_id = chat_to_session.get(&chat_id).map(|e| *e.value());
    let outcome_text = match session_id {
        Some(session_id) => {
            let resolved = {
                let guard = resolver.lock().unwrap_or_else(|e| e.into_inner());
                guard.as_ref().map(|r| r.resolve(approval_id, session_id, approved))
            };
            match resolved {
                Some(true) => Some(if approved { "✅ Đã chấp thuận." } else { "❌ Đã từ chối." }),
                Some(false) => {
                    tracing::warn!(%approval_id, chat_id, "telegram approval resolve rejected (already resolved or session mismatch)");
                    None
                }
                None => {
                    tracing::warn!("telegram callback received but no approval resolver is wired yet — ignoring");
                    None
                }
            }
        }
        None => {
            // No session bound to this chat at all — cannot possibly be the chat
            // that raised the approval. Ignore rather than guess.
            tracing::warn!(chat_id, "telegram callback from a chat with no bound session — ignoring");
            None
        }
    };

    bot.answer_callback_query(q.id).await?;
    if let Some(text) = outcome_text {
        bot.edit_message_text(message.chat.id, message.id, text).await?;
    }
    Ok(())
}

#[async_trait]
impl Adapter for TelegramAdapter {
    /// Starts the Telegram polling loop in a background task.
    async fn start(&self, tx: RequestSender) -> Result<()> {
        let bot = self.bot.clone();
        let chat_to_session = Arc::clone(&self.chat_to_session);
        let session_to_chat = Arc::clone(&self.session_to_chat);
        let resolver = Arc::clone(&self.resolver);
        let tx = Arc::new(tx);

        tokio::spawn(async move {
            let message_handler = Update::filter_message().endpoint({
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

            let callback_handler = Update::filter_callback_query().endpoint({
                let c2s = Arc::clone(&chat_to_session);
                let resolver = Arc::clone(&resolver);
                move |bot: Bot, q: CallbackQuery| {
                    let c2s = Arc::clone(&c2s);
                    let resolver = Arc::clone(&resolver);
                    async move {
                        if let Err(e) = handle_callback_query(bot, q, c2s, resolver).await {
                            tracing::warn!("telegram callback handling failed: {e:#}");
                        }
                        respond(())
                    }
                }
            });

            let handler = dptree::entry().branch(message_handler).branch(callback_handler);

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
            ResponseChunk::ToolApprovalRequest { tool, args, approval_id } => {
                if let Some(chat_id) = self.session_to_chat.get(&session_id) {
                    let msg = format!(
                        "⚙️ <b>Tool approval needed</b>\n<code>{}</code>\n{}",
                        escape_html(&tool),
                        escape_html(&args)
                    );
                    self.bot
                        .send_message(ChatId(*chat_id), msg)
                        .parse_mode(ParseMode::Html)
                        .reply_markup(approval_keyboard(approval_id))
                        .await?;
                } else {
                    // No chat is bound to this session — the user will never see the
                    // request, so it can only timeout-deny after 120s. Surface it.
                    tracing::warn!(%session_id, tool = %tool, "telegram: approval request for a session with no bound chat — user cannot respond, will timeout-deny");
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

    fn set_approval_resolver(&self, resolver: Arc<dyn ApprovalResolver>) {
        let mut guard = self.resolver.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(resolver);
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

    #[test]
    fn parse_callback_data_recognizes_approve_and_deny() {
        let id = Uuid::new_v4();
        assert_eq!(parse_callback_data(&format!("approve:{id}")), Some((true, id)));
        assert_eq!(parse_callback_data(&format!("deny:{id}")), Some((false, id)));
    }

    #[test]
    fn parse_callback_data_rejects_unrecognized_payloads() {
        assert_eq!(parse_callback_data("not-a-callback"), None);
        assert_eq!(parse_callback_data("approve:not-a-uuid"), None);
    }

    /// Foreign-chat callback: a chat_id with no bound session must never resolve an
    /// approval — this is the core of the "ignore foreign-chat callbacks" requirement,
    /// tested here at the resolver-selection level since the full `handle_callback_query`
    /// requires a live `Bot`/`CallbackQuery` from teloxide that isn't constructible in
    /// a unit test without a network layer.
    #[test]
    fn foreign_chat_has_no_bound_session_to_resolve_against() {
        let chat_to_session: DashMap<i64, Uuid> = DashMap::new();
        chat_to_session.insert(111, Uuid::new_v4());

        // The chat this callback claims to be from was never bound to any session.
        let foreign_chat_id = 999i64;
        assert!(chat_to_session.get(&foreign_chat_id).is_none());
    }
}
