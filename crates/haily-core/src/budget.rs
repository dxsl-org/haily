/// Token-budgeted context fitting — replaces the old fixed 15-turn history window.
///
/// KISS/v1: no LLM summarization (see research report 03) — history that doesn't fit
/// is hard-dropped, oldest first. Revisit only if telemetry shows real information loss.
use haily_llm::Message;

/// Estimate token count for a piece of text without a tokenizer round-trip.
///
/// `chars/3` — deliberately conservative (overcounts English, roughly tracks
/// Vietnamese BPE inflation per research report 03 §A2). Overcounting is safe (drops
/// one extra old turn); undercounting is not (blows the context window mid-generation).
pub fn estimate(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}

/// Governs how much of a backend's context window is available for prompt history
/// versus reserved for the model's own output.
pub struct TokenBudget {
    /// Total context window (tokens) for the active LLM backend.
    pub context_window: u32,
    /// Fraction of `context_window` reserved for the model's response — never
    /// counted as available for prompt/history content.
    pub output_reserve_frac: f64,
}

impl TokenBudget {
    pub fn new(context_window: u32) -> Self {
        Self { context_window, output_reserve_frac: 0.25 }
    }

    /// Tokens available for prompt content (system + history + current turn).
    pub(crate) fn prompt_budget(&self) -> usize {
        let reserve = (self.context_window as f64 * self.output_reserve_frac) as u32;
        self.context_window.saturating_sub(reserve) as usize
    }

    /// Selects which `prior_history` messages fit alongside the pinned `system`
    /// message and `current_turn` messages, within this budget.
    ///
    /// PINNING RULE (non-negotiable — see phase-05 red-team note): `system` and every
    /// message in `current_turn` are never dropped, regardless of budget. Trimming a
    /// current turn's own earlier tool results would make the model repeat or
    /// contradict tool call #1 while deciding call #4 — `LoopGuard` only catches
    /// identical *consecutive* calls, so it would not catch that regression.
    ///
    /// Only `prior_history` is eligible for dropping. It is walked newest-first,
    /// accumulating messages while they still fit; the first message that would
    /// overflow the remaining budget stops the walk — everything older is dropped.
    /// Order in the returned `Vec` is chronological (oldest kept turn first), matching
    /// the shape the LLM message array already expects.
    pub fn fit_messages(
        &self,
        system: &Message,
        prior_history: &[Message],
        current_turn: &[Message],
    ) -> Vec<Message> {
        let budget = self.prompt_budget();

        let pinned_cost = estimate(&system.content)
            + current_turn.iter().map(|m| estimate(&m.content)).sum::<usize>();

        // Pinned content alone already meets or exceeds budget: no room for any prior
        // history. Correctness over aesthetics — never drop pinned content to compensate.
        let mut remaining = budget.saturating_sub(pinned_cost);

        let mut kept_newest_first: Vec<&Message> = Vec::new();
        for msg in prior_history.iter().rev() {
            let cost = estimate(&msg.content);
            if cost > remaining {
                break;
            }
            remaining -= cost;
            kept_newest_first.push(msg);
        }

        let mut out = Vec::with_capacity(1 + kept_newest_first.len() + current_turn.len());
        out.push(system.clone());
        out.extend(kept_newest_first.into_iter().rev().cloned());
        out.extend(current_turn.iter().cloned());
        out
    }

    /// Re-fit an already-flattened `[system, ...prior_history, ...current_turn]`
    /// message list in place — used at the top of the agent tool loop, where
    /// `messages` is a single growing `Vec` rather than separate slices.
    ///
    /// `pinned_tail_len` is the number of trailing messages (the user message plus
    /// every assistant tool-call / `<tool_result>` message pushed since it) that
    /// belong to the current turn and must never be trimmed — see `fit_messages`'s
    /// pinning rule. Called before every LLM request in the loop so an accumulating
    /// `<tool_result>` payload triggers history trimming on the NEXT call rather
    /// than only once at turn start.
    ///
    /// No-op (returns the input unchanged) if `messages` is empty or shorter than
    /// `pinned_tail_len + 1` (no room for a distinct system message) — callers
    /// always satisfy this by construction, but a defensive no-op is cheaper and
    /// safer than panicking on a slicing bug.
    pub fn refit(&self, messages: &[Message], pinned_tail_len: usize) -> Vec<Message> {
        if messages.len() <= pinned_tail_len {
            return messages.to_vec();
        }
        let system = &messages[0];
        let prior_history = &messages[1..messages.len() - pinned_tail_len];
        let current_turn = &messages[messages.len() - pinned_tail_len..];
        self.fit_messages(system, prior_history, current_turn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_llm::Role;

    fn msg(role: Role, content: impl Into<String>) -> Message {
        Message { role, content: content.into() }
    }

    fn user_history(n: usize, body_chars: usize) -> Vec<Message> {
        (0..n)
            .map(|i| msg(Role::User, format!("turn-{i}-{}", "x".repeat(body_chars))))
            .collect()
    }

    #[test]
    fn estimate_uses_chars_div_3_rounded_up() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("abc"), 1);
        assert_eq!(estimate("abcd"), 2); // ceil(4/3) = 2
        assert_eq!(estimate("abcdef"), 2);
    }

    #[test]
    fn estimate_does_not_undercount_mixed_vietnamese_english_sample() {
        // Mixed VN/EN sample resembling real system-prompt content. Vietnamese
        // diacritics are multi-byte UTF-8 but single `char`s, so this just checks the
        // heuristic produces a reasonable (non-zero, proportionate) lower bound — the
        // real safety net is the debug-log comparison against llama.rs's exact count.
        let sample = "Khi cần dùng tool, output ĐÚNG format này. Please use the tool_call format exactly.";
        let est = estimate(sample);
        let chars = sample.chars().count();
        // chars/3 must never be smaller than chars/4 (a laxer, more permissive divisor)
        // — i.e. our estimator is at least as conservative as the naive English baseline.
        assert!(est >= chars.div_ceil(4));
        assert!(est > 0);
    }

    #[test]
    fn fits_within_budget_when_everything_is_small() {
        let budget = TokenBudget::new(1000);
        let system = msg(Role::System, "sys");
        let history = user_history(3, 5);
        let current = vec![msg(Role::User, "hello")];

        let fitted = budget.fit_messages(&system, &history, &current);

        // system + all history + current turn, nothing dropped.
        assert_eq!(fitted.len(), 1 + history.len() + current.len());
        assert_eq!(fitted[0].content, "sys");
    }

    #[test]
    fn hundred_turn_history_is_trimmed_and_newest_survive() {
        // Small window forces trimming; each history message ~ (turn-N-xxxxxxxxxx) chars.
        let budget = TokenBudget::new(200); // prompt_budget = 150 tokens
        let system = msg(Role::System, "s");
        let history = user_history(100, 20); // ~30 chars each -> ~10 tokens each
        let current = vec![msg(Role::User, "current message")];

        let fitted = budget.fit_messages(&system, &history, &current);

        // Never exceeds the prompt budget.
        let total: usize = fitted.iter().map(|m| estimate(&m.content)).sum();
        assert!(total <= 150, "fitted set must respect prompt budget, got {total}");

        // System is present.
        assert_eq!(fitted.first().unwrap().content, "s");
        // Current turn is present (last message).
        assert_eq!(fitted.last().unwrap().content, "current message");
        // Not all 100 history turns survived (proves trimming happened).
        assert!(fitted.len() < 1 + history.len() + current.len());
        // The newest surviving prior-history message must be the most recent one
        // present, i.e. dropped messages are strictly from the oldest end.
        let history_kept: Vec<&Message> =
            fitted[1..fitted.len() - current.len()].iter().collect();
        if let Some(newest_kept) = history_kept.last() {
            assert!(newest_kept.content.starts_with("turn-99") || history_kept.len() < history.len());
        }
        // Every kept history message must be a suffix (newest contiguous run) of the
        // original history — i.e. index order is preserved and it's the tail end.
        let kept_indices: Vec<usize> = history_kept
            .iter()
            .map(|m| {
                m.content
                    .trim_start_matches("turn-")
                    .split('-')
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .collect();
        for w in kept_indices.windows(2) {
            assert!(w[0] < w[1], "kept history must stay in chronological order");
        }
        if let Some(&first_kept) = kept_indices.first() {
            assert_eq!(first_kept, 100 - kept_indices.len());
        }
    }

    #[test]
    fn giant_tool_result_mid_turn_keeps_system_and_current_turn_intact() {
        let budget = TokenBudget::new(4096); // prompt_budget = 3072 tokens
        let system = msg(Role::System, "system prompt block");
        let history = user_history(20, 50); // plenty of prior history competing for space
        let giant_result = "y".repeat(50_000); // 50k chars ~ 16667 tokens — dwarfs the budget
        let current = vec![
            msg(Role::User, "do the thing"),
            msg(Role::Assistant, "<tool_call>{}</tool_call>"),
            msg(Role::User, format!("<tool_result>{giant_result}</tool_result>")),
        ];

        let fitted = budget.fit_messages(&system, &history, &current);

        // System survives.
        assert_eq!(fitted[0].content, "system prompt block");
        // All current-turn messages survive, including the giant result, even though
        // pinned cost alone may exceed the nominal budget.
        let tail = &fitted[fitted.len() - current.len()..];
        assert_eq!(tail, current.as_slice());
        // Prior history was trimmed away (likely to zero) to make room, but never the
        // pinned content.
        assert!(fitted.len() <= 1 + history.len() + current.len());
    }

    #[test]
    fn four_tool_call_turn_earlier_same_turn_results_survive_pinning() {
        let budget = TokenBudget::new(2048);
        let system = msg(Role::System, "sys");
        let history = user_history(10, 100);
        // Simulate 4 tool calls: user msg + 4x (assistant call + tool result).
        let mut current = vec![msg(Role::User, "multi-step task")];
        for i in 0..4 {
            current.push(msg(Role::Assistant, format!("<tool_call>{{\"tool\":\"t{i}\"}}</tool_call>")));
            current.push(msg(Role::User, format!("<tool_result>{{\"result\":\"call-{i}-data\"}}</tool_result>")));
        }

        let fitted = budget.fit_messages(&system, &history, &current);

        // Every current-turn message, including the FIRST tool call's result, survives.
        let tail = &fitted[fitted.len() - current.len()..];
        assert_eq!(tail, current.as_slice());
        assert!(tail.iter().any(|m| m.content.contains("call-0-data")), "earliest same-turn tool result must survive");
        assert!(tail.iter().any(|m| m.content.contains("call-3-data")), "latest same-turn tool result must survive");
    }

    #[test]
    fn empty_prior_history_is_a_noop() {
        let budget = TokenBudget::new(500);
        let system = msg(Role::System, "sys");
        let current = vec![msg(Role::User, "hi")];

        let fitted = budget.fit_messages(&system, &[], &current);

        assert_eq!(fitted.len(), 2);
        assert_eq!(fitted[0].content, "sys");
        assert_eq!(fitted[1].content, "hi");
    }

    #[test]
    fn output_reserve_shrinks_available_prompt_budget() {
        let mut budget = TokenBudget::new(1000);
        budget.output_reserve_frac = 0.9; // only 10% (100 tokens) left for prompt content
        let system = msg(Role::System, "sys");
        let history = user_history(50, 100); // large history competing for the tiny budget
        let current = vec![msg(Role::User, "hi")];

        let fitted = budget.fit_messages(&system, &history, &current);
        let total: usize = fitted.iter().map(|m| estimate(&m.content)).sum();
        assert!(total <= 100);
    }

    #[test]
    fn cloud_32k_budget_admits_far_more_history_than_llama_8k() {
        let system = msg(Role::System, "sys");
        let history = user_history(200, 80);
        let current = vec![msg(Role::User, "hi")];

        let llama_budget = TokenBudget::new(8192);
        let cloud_budget = TokenBudget::new(32_000);

        let llama_fitted = llama_budget.fit_messages(&system, &history, &current);
        let cloud_fitted = cloud_budget.fit_messages(&system, &history, &current);

        assert!(
            cloud_fitted.len() >= llama_fitted.len(),
            "larger context window must admit at least as much history"
        );
    }

    // -----------------------------------------------------------------------
    // `refit` — the agent-loop entry point over a flattened message Vec.
    // -----------------------------------------------------------------------

    #[test]
    fn refit_trims_prior_history_but_preserves_pinned_tail() {
        let budget = TokenBudget::new(200); // prompt_budget = 150 tokens
        let mut messages = vec![msg(Role::System, "s")];
        messages.extend(user_history(50, 20)); // large prior history to force trimming
        // Pinned tail: user message + 4 tool-call/result pairs (9 messages).
        let pinned_tail = {
            let mut t = vec![msg(Role::User, "multi-step task")];
            for i in 0..4 {
                t.push(msg(Role::Assistant, format!("<tool_call>{{\"tool\":\"t{i}\"}}</tool_call>")));
                t.push(msg(Role::User, format!("<tool_result>call-{i}-data</tool_result>")));
            }
            t
        };
        messages.extend(pinned_tail.clone());

        let refitted = budget.refit(&messages, pinned_tail.len());

        // Pinned tail survives byte-for-byte, in order.
        let tail = &refitted[refitted.len() - pinned_tail.len()..];
        assert_eq!(tail, pinned_tail.as_slice());
        // System survives.
        assert_eq!(refitted[0].content, "s");
        // Overall result respects the budget.
        let total: usize = refitted.iter().map(|m| estimate(&m.content)).sum();
        assert!(total <= 150);
    }

    #[test]
    fn refit_is_noop_when_messages_no_longer_than_pinned_tail() {
        let budget = TokenBudget::new(1000);
        let messages = vec![msg(Role::System, "s"), msg(Role::User, "hi")];
        // pinned_tail_len >= messages.len() → defensive no-op path.
        let refitted = budget.refit(&messages, 5);
        assert_eq!(refitted, messages);
    }

    #[test]
    fn refit_growing_tool_result_triggers_progressively_more_trimming() {
        // Simulates the agent loop: each iteration appends another tool_result and
        // calls refit again. Later calls must trim at least as much prior history as
        // earlier ones, since the pinned cost only grows.
        let budget = TokenBudget::new(300);
        let mut messages = vec![msg(Role::System, "s")];
        messages.extend(user_history(30, 15));

        let mut pinned_tail = vec![msg(Role::User, "task")];
        let after_first = budget.refit(&messages, pinned_tail.len());
        let history_kept_first = after_first.len() - 1 - pinned_tail.len();

        // Grow the pinned tail with a big tool result and re-fit against the ORIGINAL
        // full message list (mirroring the loop, which re-fits from the source of
        // truth each time, not from the previously-trimmed output).
        pinned_tail.push(msg(Role::Assistant, "<tool_call>{}</tool_call>"));
        pinned_tail.push(msg(Role::User, format!("<tool_result>{}</tool_result>", "z".repeat(2000))));
        let mut messages_v2 = vec![msg(Role::System, "s")];
        messages_v2.extend(user_history(30, 15));
        messages_v2.extend(pinned_tail.clone());

        let after_second = budget.refit(&messages_v2, pinned_tail.len());
        let history_kept_second = after_second.len() - 1 - pinned_tail.len();

        assert!(
            history_kept_second <= history_kept_first,
            "growing pinned tool-result cost must not increase surviving prior history"
        );
    }
}
