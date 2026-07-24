use anyhow::Result;
use haily_llm::StreamChunk;
use haily_types::ResponseChunk;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::tag_matcher;

/// Splits `buffer` into `(emit, hold)` at the hold-back boundary: `hold` is the
/// longest trailing suffix that could still extend into a recognized tool tag
/// (`<tool_call>`/`<tool_result>`, whitespace/case tolerant — see `tag_matcher`) if
/// more text arrives; `emit` is everything before it, safe to show the user now.
///
/// This only answers "could the tail still become an OPENING tag" — it does not by
/// itself know about an already-confirmed, still-open tag body (that's
/// `stream_llm_response`'s `in_tag` state, tracked separately). Pure function so the
/// exhaustive boundary cases (tag split across chunks, mid-chunk, variant tags, no
/// tag) are unit-testable without a channel.
fn split_safe(buffer: &str) -> (&str, &str) {
    let hold_len = tag_matcher::holdback_len(buffer);
    buffer.split_at(buffer.len() - hold_len)
}

/// Consumes a `complete_stream` channel, forwarding safe text increments to `tx` as
/// `ResponseChunk::Text` while withholding any tool-tag body in full — from the
/// moment an opening tag is confirmed until its matching closing tag is found — and
/// returns the FULL accumulated raw response text — identical in shape to what
/// `complete()` would have returned, so callers (`run_turn`'s tool-call loop) can
/// keep parsing/dispatching against the complete string exactly as before.
///
/// Two-state machine over the growing buffer:
/// - `Scanning`: no confirmed open tag yet. `split_safe` withholds only the trailing
///   prefix-of-a-tag; once a FULL open tag is confirmed inside the withheld portion,
///   switch to `InTag` and withhold everything from that tag's `<` onward.
/// - `InTag`: withhold everything (never call `split_safe`, never emit) until the
///   matching close tag is found in the buffer, then resume `Scanning` from just
///   past the close tag.
///
/// SECURITY: text is only ever forwarded to `tx` while `Scanning` and past
/// `split_safe`'s hold-back boundary — this is the boundary that stops partial (or
/// even complete but unapproved) tool-call JSON from ever reaching the user before
/// `tool_call::dispatch`'s approval gate runs (see phase-06 spec's Security
/// Considerations).
///
/// Cancellation: selects on `cancel.cancelled()` alongside `rx.recv()` so a fired
/// token ends consumption within one channel-poll, without waiting for the
/// producer (llama's blocking decode loop / the cloud SSE task) to notice on its own
/// — dropping `rx` here is itself the second half of the cancellation signal those
/// producers watch for (see `llama.rs`/`cloud.rs` doc comments).
///
/// Returns `(full_text, total_tokens, prompt_tokens)`. CONTRACT (Phase 8, C2 —
/// supersedes the prior H2-review note): `prompt_tokens` is `StreamChunk::Done`'s own
/// provenance signal — `Some` only on the llama.cpp backend, which tokenizes the
/// prompt up front and increments `total_tokens` once per actually-decoded token, so
/// BOTH numbers are genuine measurements there. It is `None` on the cloud SSE
/// backend, which counts `Delta` EVENTS, not tokens (a provider may batch several
/// tokens into one delta) and exposes no real `usage` field on any dialect this crate
/// speaks. Callers MUST gate trusting `total_tokens` as a completion-token count on
/// `prompt_tokens.is_some()` — never persist `total_tokens` as
/// `TraceMetrics::completion_tokens` when `prompt_tokens` is `None` (see this
/// function's main-turn caller and the cloud-NULL honesty tests in
/// `outcome_signal_tests`).
pub(super) async fn stream_llm_response(
    rx: &mut mpsc::Receiver<StreamChunk>,
    tx: &mpsc::Sender<ResponseChunk>,
    cancel: &CancellationToken,
) -> Result<(String, u32, Option<u32>)> {
    let mut full = String::new();
    // Buffer of bytes not yet flushed to `tx`. While `Scanning`, holds only the
    // tail that might still become a tag; while `InTag`, holds the entire withheld
    // tag body seen so far (never flushed until the close tag resolves it).
    let mut pending = String::new();
    let mut in_tag = false;

    loop {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(anyhow::anyhow!("turn cancelled"));
            }
            chunk = rx.recv() => chunk,
        };

        match chunk {
            Some(StreamChunk::Token(piece)) => {
                full.push_str(&piece);
                pending.push_str(&piece);

                // Drain as many complete open/close tag transitions as the buffer
                // currently supports — a single Token piece could conceivably close
                // out a tag AND open another in pathological model output.
                loop {
                    if in_tag {
                        match tag_matcher::find_next_tag(&pending, 0) {
                            Some(m) if m.closing => {
                                // Matching close found: the whole tag body (open
                                // through close) stays withheld from `tx` forever —
                                // only text AFTER it re-enters the safe-to-emit path.
                                pending = pending[m.end..].to_string();
                                in_tag = false;
                                continue; // re-scan: more content may already be buffered
                            }
                            _ => break, // still inside the tag body — wait for more input
                        }
                    } else {
                        // A fully-formed OPEN tag anywhere in `pending` must be
                        // checked directly — `split_safe`/`holdback_len` only reasons
                        // about an as-yet-*unresolved* trailing prefix, so a tag that
                        // is already fully closed within `pending` (e.g. an entire
                        // `<tool_call>...</tool_call>` arriving in one Token piece)
                        // would otherwise sail through as "nothing pending" and leak.
                        match tag_matcher::find_next_tag(&pending, 0) {
                            Some(m) if !m.closing => {
                                let before = &pending[..m.start];
                                if !before.is_empty() {
                                    let text = before.to_string();
                                    let _ = tx.send(ResponseChunk::Text(text)).await;
                                }
                                pending = pending[m.start..].to_string();
                                in_tag = true;
                                continue; // re-scan in InTag state immediately
                            }
                            Some(m) => {
                                // A stray CLOSING tag before any open tag — routine when a
                                // weak model echoes the `</tool_result>` framing injected
                                // into context each round. Emit the safe text before it,
                                // DROP the stray token (never shown to the user), and keep
                                // scanning the remainder: a genuine `<tool_call>` block can
                                // follow in the same buffer and must not be handed to the
                                // suffix-only `split_safe`, which would leak it verbatim.
                                let before = &pending[..m.start];
                                if !before.is_empty() {
                                    let text = before.to_string();
                                    let _ = tx.send(ResponseChunk::Text(text)).await;
                                }
                                pending = pending[m.end..].to_string();
                                continue;
                            }
                            None => {
                                // No confirmed tag yet — fall back to the trailing-prefix
                                // hold-back for the still-ambiguous tail (e.g. a lone '<'
                                // or a partial tag name).
                                let (emit, hold) = split_safe(&pending);
                                if !emit.is_empty() {
                                    let text = emit.to_string();
                                    let _ = tx.send(ResponseChunk::Text(text)).await;
                                    pending = hold.to_string();
                                }
                                break;
                            }
                        }
                    }
                }
            }
            Some(StreamChunk::Done {
                total_tokens,
                prompt_tokens,
            }) => {
                // Any residual `pending` text at clean end-of-stream was never
                // confirmed to close out a tag (a real closed tag would already have
                // been drained above) — either an incomplete tag prefix (e.g. a lone
                // trailing '<') or, rarer, an unterminated `<tool_call>` the model
                // never closed. Either way it's already included in `full` for the
                // caller's `parse_tool_call`/`strip_tool_markup` pass, so it must NOT
                // be flushed here too — an unterminated tag left in `pending` must
                // stay invisible to the user (the security invariant this function
                // exists for), and a plain incomplete prefix will be re-rendered by
                // `strip_tool_markup` at the loop's end instead.
                return Ok((full, total_tokens, prompt_tokens));
            }
            Some(StreamChunk::Error(msg)) => {
                return Err(anyhow::anyhow!("{msg}"));
            }
            None => {
                // Channel closed without a Done/Error — treat as an abnormal end
                // rather than silently returning a truncated success.
                return Err(anyhow::anyhow!(
                    "LLM stream ended without a completion signal"
                ));
            }
        }
    }
}

#[cfg(test)]
mod streaming_tests {
    //! Phase 6 — hold-back streaming. `split_safe` is exhaustively unit-tested here
    //! (pure function, no async needed); `stream_llm_response` is tested against a
    //! real `mpsc` channel fed canned `StreamChunk`s to prove the end-to-end
    //! consumer never lets tag bytes reach `tx` and still returns the full text for
    //! `parse_tool_call`.
    use super::*;
    use crate::tool_call;

    #[test]
    fn split_safe_emits_everything_when_no_tag_present() {
        let (emit, hold) = split_safe("hello, how can I help?");
        assert_eq!(emit, "hello, how can I help?");
        assert_eq!(hold, "");
    }

    #[test]
    fn split_safe_withholds_tag_split_mid_word() {
        let (emit, hold) = split_safe("here you go <tool_c");
        assert_eq!(emit, "here you go ");
        assert_eq!(hold, "<tool_c");
    }

    #[test]
    fn split_safe_withholds_full_tag_awaiting_close_bracket() {
        let (emit, hold) = split_safe("ok <tool_call");
        assert_eq!(emit, "ok ");
        assert_eq!(hold, "<tool_call");
    }

    #[test]
    fn split_safe_emits_full_tag_once_confirmed_complete() {
        // A CLOSED tag is not held back by split_safe itself — the caller
        // (`stream_llm_response`) still accumulates it into `full` for
        // `parse_tool_call`, but split_safe's own contract is purely "could this
        // still extend into a tag", which a terminated `>` answers no to.
        let (emit, hold) = split_safe("<tool_call>{}</tool_call>");
        assert_eq!(emit, "<tool_call>{}</tool_call>");
        assert_eq!(hold, "");
    }

    #[test]
    fn split_safe_handles_variant_tags_case_and_whitespace() {
        let (emit, hold) = split_safe("answer <Tool_Call ");
        assert_eq!(emit, "answer ");
        assert_eq!(hold, "<Tool_Call ");
    }

    #[test]
    fn split_safe_recovers_once_bracket_content_diverges_from_any_tag() {
        // "<b>" cannot extend into tool_call/tool_result — safe to emit in full.
        let (emit, hold) = split_safe("some <b>html</b> text");
        assert_eq!(emit, "some <b>html</b> text");
        assert_eq!(hold, "");
    }

    /// Feeds `pieces` through `stream_llm_response` as `StreamChunk::Token`s
    /// followed by `Done`, and returns `(visible_text_sent_to_tx, full_return_value)`.
    async fn run_stream(pieces: &[&str]) -> (String, String) {
        let (llm_tx, mut llm_rx) = mpsc::channel(64);
        for p in pieces {
            llm_tx
                .send(StreamChunk::Token(p.to_string()))
                .await
                .unwrap();
        }
        llm_tx
            .send(StreamChunk::Done {
                total_tokens: pieces.len() as u32,
                prompt_tokens: None,
            })
            .await
            .unwrap();
        drop(llm_tx);

        let (user_tx, mut user_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let (full, _total_tokens, _prompt_tokens) =
            stream_llm_response(&mut llm_rx, &user_tx, &cancel)
                .await
                .unwrap();
        drop(user_tx);

        let mut visible = String::new();
        while let Some(chunk) = user_rx.recv().await {
            if let ResponseChunk::Text(t) = chunk {
                visible.push_str(&t);
            }
        }
        (visible, full)
    }

    #[tokio::test]
    async fn tool_call_split_across_three_chunks_never_leaks_to_user() {
        // "<tool_call>{"tool":"x","args":{}}</tool_call>" split across 3 arbitrary
        // chunk boundaries, including mid-tag-name.
        let (visible, full) = run_stream(&[
            "Để mình kiểm tra nhé. <tool_",
            "call>{\"tool\":\"x\",\"args\":{}}</tool_c",
            "all>",
        ])
        .await;

        assert_eq!(
            visible, "Để mình kiểm tra nhé. ",
            "zero tag bytes must reach the user"
        );
        assert!(
            !visible.contains('<'),
            "no angle bracket of any kind may leak"
        );
        let (tool, _args) = tool_call::parse_tool_call(&full).expect("full text must still parse");
        assert_eq!(tool, "x");
    }

    #[tokio::test]
    async fn tag_mid_chunk_is_withheld_from_first_safe_boundary() {
        let (visible, full) =
            run_stream(&["prefix <tool_call>{\"tool\":\"y\"}</tool_call> ignored-suffix"]).await;
        // Only the text strictly before the tag is visible; everything from '<'
        // onward in this single chunk is held back until the loop-level buffer
        // resolves it, but stream_llm_response's job is only to never leak tag
        // bytes — the trailing "ignored-suffix" after a still-open call is legitimately
        // buffered until Done, at which point `full` (not `visible`) carries it.
        assert_eq!(visible, "prefix ");
        assert!(!visible.contains("tool_call"));
        let (tool, _) = tool_call::parse_tool_call(&full).expect("must parse");
        assert_eq!(tool, "y");
    }

    #[tokio::test]
    async fn variant_tag_with_trailing_space_is_withheld_and_parses() {
        let (visible, full) = run_stream(&["ok <tool_call >{\"tool\":\"z\"}</ tool_call>"]).await;
        assert_eq!(visible, "ok ");
        assert!(!visible.to_ascii_lowercase().contains("tool_call"));
        let (tool, _) = tool_call::parse_tool_call(&full).expect("variant tags must still parse");
        assert_eq!(tool, "z");
    }

    #[tokio::test]
    async fn mixed_case_variant_tag_is_withheld_and_parses() {
        let (visible, full) = run_stream(&["<Tool_Call>{\"tool\":\"w\"}</Tool_Call>"]).await;
        assert_eq!(visible, "");
        let (tool, _) =
            tool_call::parse_tool_call(&full).expect("mixed-case tags must still parse");
        assert_eq!(tool, "w");
    }

    #[tokio::test]
    async fn stray_closing_tag_before_a_real_call_never_leaks_the_block() {
        // The Phase-6 review's CRITICAL case: a stray `</tool_result>` (routinely echoed
        // from injected framing) appears before a genuine `<tool_call>` in the SAME
        // chunk. The scanner must skip the stray close and withhold the whole call —
        // never hand it to the suffix-only hold-back, which would stream the JSON args.
        let (visible, full) = run_stream(&[
            r#"kết quả </tool_result> rồi <tool_call>{"tool":"x","args":{"path":"/home/secret"}}</tool_call>"#,
        ])
        .await;
        assert!(
            !visible.contains("tool_call"),
            "tool-call tag/JSON must not leak: {visible:?}"
        );
        assert!(
            !visible.contains("/home/secret"),
            "tool args must not leak: {visible:?}"
        );
        // The real call is still recoverable from `full` for dispatch.
        let (tool, _) =
            tool_call::parse_tool_call(&full).expect("real call must still parse from full");
        assert_eq!(tool, "x");
    }

    #[tokio::test]
    async fn plain_text_with_no_tag_streams_immediately_and_completely() {
        let (visible, full) = run_stream(&["Xin ", "chào, ", "hôm nay trời đẹp."]).await;
        assert_eq!(visible, "Xin chào, hôm nay trời đẹp.");
        assert_eq!(full, "Xin chào, hôm nay trời đẹp.");
    }

    #[tokio::test]
    async fn stream_error_after_partial_text_returns_err_with_partial_visible() {
        let (llm_tx, mut llm_rx) = mpsc::channel(64);
        llm_tx
            .send(StreamChunk::Token("partial answer".to_string()))
            .await
            .unwrap();
        llm_tx
            .send(StreamChunk::Error("backend disconnected".to_string()))
            .await
            .unwrap();
        drop(llm_tx);

        let (user_tx, mut user_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let result = stream_llm_response(&mut llm_rx, &user_tx, &cancel).await;
        drop(user_tx);

        assert!(
            result.is_err(),
            "a stream error must surface as Err, not a truncated Ok"
        );

        let mut visible = String::new();
        while let Some(ResponseChunk::Text(t)) = user_rx.recv().await {
            visible.push_str(&t);
        }
        assert_eq!(
            visible, "partial answer",
            "text streamed before the error must still have been delivered"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_consumption_promptly() {
        let (_llm_tx, mut llm_rx) = mpsc::channel::<StreamChunk>(64); // never sends — only cancel ends this
        let (user_tx, _user_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream_llm_response(&mut llm_rx, &user_tx, &cancel),
        )
        .await
        .expect("cancellation must end consumption promptly, not hang");

        assert!(
            result.is_err(),
            "cancellation must surface as an Err so the turn fails cleanly"
        );
    }

    /// C2 (Phase 8): `stream_llm_response` must pass `StreamChunk::Done`'s
    /// `prompt_tokens` straight through — a llama-shaped `Done` frame (`Some`) comes
    /// back `Some`, unmodified.
    #[tokio::test]
    async fn done_frame_prompt_tokens_some_is_threaded_through_unmodified() {
        let (llm_tx, mut llm_rx) = mpsc::channel(64);
        llm_tx
            .send(StreamChunk::Token("hi".to_string()))
            .await
            .unwrap();
        llm_tx
            .send(StreamChunk::Done {
                total_tokens: 3,
                prompt_tokens: Some(42),
            })
            .await
            .unwrap();
        drop(llm_tx);

        let (user_tx, mut user_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let (_full, total_tokens, prompt_tokens) =
            stream_llm_response(&mut llm_rx, &user_tx, &cancel)
                .await
                .unwrap();
        drop(user_tx);
        while user_rx.recv().await.is_some() {}

        assert_eq!(total_tokens, 3);
        assert_eq!(
            prompt_tokens,
            Some(42),
            "a llama-shaped Done frame's real prompt-token count must survive threading"
        );
    }

    /// C2 (Phase 8): a cloud-shaped `Done` frame (`prompt_tokens: None`) must stay
    /// `None` — the NULL-honesty invariant this function's contract exists to
    /// preserve (never upgraded into a fabricated number by this pass-through layer).
    #[tokio::test]
    async fn done_frame_prompt_tokens_none_stays_none() {
        let (llm_tx, mut llm_rx) = mpsc::channel(64);
        llm_tx
            .send(StreamChunk::Token("hi".to_string()))
            .await
            .unwrap();
        llm_tx
            .send(StreamChunk::Done {
                total_tokens: 3,
                prompt_tokens: None,
            })
            .await
            .unwrap();
        drop(llm_tx);

        let (user_tx, mut user_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let (_full, _total_tokens, prompt_tokens) =
            stream_llm_response(&mut llm_rx, &user_tx, &cancel)
                .await
                .unwrap();
        drop(user_tx);
        while user_rx.recv().await.is_some() {}

        assert_eq!(
            prompt_tokens, None,
            "a cloud-shaped Done frame must never be upgraded into a fabricated prompt-token count"
        );
    }
}
