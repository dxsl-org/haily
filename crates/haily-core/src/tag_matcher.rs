//! Canonical tool-tag matcher — the SINGLE source of truth for recognizing
//! `<tool_call>`/`</tool_call>`/`<tool_result>`/`</tool_result>` markup, tolerant of
//! whitespace and case variants (e.g. `<tool_call >`, `<Tool_Call>`, `< tool_call>`).
//!
//! Before this module, `parse_tool_call`, `strip_tool_markup`, and `strip_tool_tags`
//! each did an exact byte-match on the literal lowercase tag — a model emitting
//! `<tool_call >` or `<Tool_Call>` would both leak those bytes to the user (streaming
//! hold-back wouldn't recognize it as a tag) AND fail to parse (the dispatcher
//! wouldn't recognize it as a call either). Every consumer that needs to reason about
//! tool tags MUST go through this module so the accepted-tag surface never drifts
//! between the parser and the streaming hold-back — the hold-back predicate is
//! required (by the phase-06 spec) to accept a SUPERSET of what the parser accepts,
//! which is trivially true when both call the same function.

/// The four tag names this protocol recognizes, in canonical (lowercase) form.
const TAG_NAMES: [&str; 2] = ["tool_call", "tool_result"];

/// One located tag occurrence: canonical name, whether it's a closing tag, and the
/// byte range in the source string it occupies (`start..end`, end-exclusive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagMatch {
    pub name: &'static str,
    pub closing: bool,
    pub start: usize,
    pub end: usize,
}

/// Scans `text` for the next tag occurrence (of any recognized name, open or close)
/// starting at or after byte offset `from`. Returns `None` if no tag is found.
///
/// Tolerant of: leading/trailing whitespace inside the angle brackets
/// (`< tool_call >`), and case variance in the tag name (`<Tool_Call>`). This is the
/// canonical, tolerant equivalent of the regex `<\s*/?\s*tool_(call|result)\s*>`
/// (case-insensitive) — hand-rolled rather than pulling in the `regex` crate for a
/// four-tag, fixed-alphabet scan (YAGNI/KISS).
pub fn find_next_tag(text: &str, from: usize) -> Option<TagMatch> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if let Some(m) = try_match_tag_at(text, i) {
            return Some(m);
        }
        i += 1;
    }
    None
}

/// Scans forward for the next OPENING tag with canonical `name`, skipping any
/// intervening tags of a different name or direction. Returns `None` if none exists
/// at or after `from`.
///
/// Callers asking "is there a real `<tool_call>` here?" must not be fooled by a stray
/// earlier tag — e.g. a `</tool_result>` a weak model echoes from the tool-result
/// framing injected into context each round, appearing before the genuine
/// `<tool_call>`. A bare `find_next_tag(..).filter(..)` returns that stray tag and
/// hides the real one; this walks past non-matching tags instead.
pub fn find_next_open_tag(text: &str, from: usize, name: &str) -> Option<TagMatch> {
    let mut cursor = from;
    while let Some(m) = find_next_tag(text, cursor) {
        if !m.closing && m.name == name {
            return Some(m);
        }
        cursor = m.end; // `end > start >= cursor`, so this always advances
    }
    None
}

/// Scans forward for the next CLOSING tag with canonical `name`, skipping any
/// intervening tags of a different name or direction. Used to find the close that
/// matches an already-located open tag without being derailed by an interleaved tag.
pub fn find_next_close_tag(text: &str, from: usize, name: &str) -> Option<TagMatch> {
    let mut cursor = from;
    while let Some(m) = find_next_tag(text, cursor) {
        if m.closing && m.name == name {
            return Some(m);
        }
        cursor = m.end;
    }
    None
}

/// Attempts to parse a tag starting exactly at byte offset `start` (which must point
/// at a `<`). Returns `None` if the bytes at `start` don't form a recognized tag.
fn try_match_tag_at(text: &str, start: usize) -> Option<TagMatch> {
    let rest = &text[start..];
    let mut chars = rest.char_indices().peekable();
    let (_, open) = chars.next()?; // '<'
    debug_assert_eq!(open, '<');

    let mut cursor = 1; // byte offset within `rest`, past '<'
                        // Optional '/' for a closing tag, optionally preceded/followed by whitespace.
    cursor += skip_ws(&rest[cursor..]);
    let closing = rest[cursor..].starts_with('/');
    if closing {
        cursor += 1;
    }
    cursor += skip_ws(&rest[cursor..]);

    // Match one of the known tag names, case-insensitively, at `cursor`.
    let name = TAG_NAMES.iter().find(|n| {
        rest[cursor..]
            .get(..n.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(n))
    })?;
    cursor += name.len();

    cursor += skip_ws(&rest[cursor..]);
    if !rest[cursor..].starts_with('>') {
        return None;
    }
    cursor += 1; // '>'

    Some(TagMatch {
        name,
        closing,
        start,
        end: start + cursor,
    })
}

/// Returns the number of leading ASCII whitespace bytes in `s`.
fn skip_ws(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_whitespace).count()
}

/// Longest suffix of `buffer` that could still extend into a recognized opening tag
/// (`<tool_call>` / `<tool_result>`, tolerant of whitespace/case) if more bytes
/// arrive. Used by the streaming hold-back: text up to `buffer.len() - hold_len` is
/// safe to emit immediately; the remaining tail must wait for the next chunk.
///
/// Only opening tags are considered a hold-back risk — once inside a confirmed
/// `<tool_call>` block the caller is already in full-buffering mode (see
/// `agent.rs::split_safe`'s caller), so a partial *closing* tag at the tail of
/// buffered-but-not-yet-user-visible text needs no separate prefix check here.
///
/// Returns 0 if no suffix of `buffer` is a prefix (under whitespace/case tolerance)
/// of any recognized opening tag.
pub fn holdback_len(buffer: &str) -> usize {
    // Candidate prefixes of a tolerant opening tag: '<', "</" is a closing tag so it's
    // excluded from *opening*-tag lookahead, but a bare '<' is ambiguous (could become
    // '<tool_call>' OR '</tool_call>' is impossible without more text after '<' — a
    // lone '<' must still be held back because '<tool_call>' starts with it).
    let n = buffer.chars().count();
    for take in (1..=n).rev() {
        let suffix_start = buffer
            .char_indices()
            .rev()
            .nth(take - 1)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let suffix = &buffer[suffix_start..];
        if is_prefix_of_open_tag(suffix) {
            return suffix.len();
        }
    }
    0
}

/// Whether `s` could be the start of a tolerant `<tool_call>`/`<tool_result>` opening
/// tag — i.e. every character in `s` matches what the canonical open-tag grammar
/// would accept at that position: `<`, then optional whitespace, then a
/// case-insensitive prefix of one of `TAG_NAMES`, then optional whitespace, then `>`.
fn is_prefix_of_open_tag(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some('<') => {}
        _ => return false,
    }
    let rest: String = chars.collect();

    // Skip leading whitespace (tag name may be preceded by spaces: "< tool_call>").
    let after_ws = rest.trim_start();
    if after_ws.is_empty() {
        return true; // "<" or "<   " — still an unresolved prefix
    }

    // The remaining text must be a case-insensitive prefix of some tag name,
    // OR a full tag name possibly followed by whitespace (awaiting '>').
    let lower = after_ws.to_ascii_lowercase();
    for name in TAG_NAMES {
        if name.starts_with(&lower) {
            return true; // still building the name itself
        }
        if let Some(after_name) = lower.strip_prefix(name) {
            // Name is complete; only whitespace may follow before '>' arrives.
            if after_name.chars().all(|c| c.is_ascii_whitespace()) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_canonical_lowercase_tags() {
        let m = find_next_tag("hello <tool_call>x", 0).unwrap();
        assert_eq!(m.name, "tool_call");
        assert!(!m.closing);
        assert_eq!(&"hello <tool_call>x"[m.start..m.end], "<tool_call>");
    }

    #[test]
    fn finds_closing_tags() {
        let m = find_next_tag("x</tool_call>y", 0).unwrap();
        assert_eq!(m.name, "tool_call");
        assert!(m.closing);
    }

    #[test]
    fn tolerates_trailing_whitespace_before_close_bracket() {
        let m = find_next_tag("<tool_call >x", 0).unwrap();
        assert_eq!(m.name, "tool_call");
        assert!(!m.closing);
        assert_eq!(&"<tool_call >x"[m.start..m.end], "<tool_call >");
    }

    #[test]
    fn tolerates_case_variants() {
        let m = find_next_tag("<Tool_Call>x", 0).unwrap();
        assert_eq!(m.name, "tool_call");
    }

    #[test]
    fn tolerates_whitespace_after_slash_in_closing_tag() {
        let m = find_next_tag("< / tool_call >x", 0).unwrap();
        assert!(m.closing);
        assert_eq!(m.name, "tool_call");
    }

    #[test]
    fn rejects_unrelated_angle_bracket_content() {
        assert!(find_next_tag("hello <b>world</b>", 0).is_none());
    }

    #[test]
    fn finds_second_occurrence_after_from_offset() {
        let text = "<tool_call>a</tool_call>";
        let first = find_next_tag(text, 0).unwrap();
        let second = find_next_tag(text, first.end).unwrap();
        assert!(second.closing);
    }

    #[test]
    fn holdback_zero_for_plain_text() {
        assert_eq!(holdback_len("hello world"), 0);
    }

    #[test]
    fn holdback_full_buffer_for_lone_angle_bracket() {
        assert_eq!(holdback_len("hi <"), 1);
    }

    #[test]
    fn holdback_grows_with_partial_tag_name() {
        assert_eq!(holdback_len("hi <tool_c"), "<tool_c".len());
    }

    #[test]
    fn holdback_covers_case_variant_prefix() {
        assert_eq!(holdback_len("hi <Tool_C"), "<Tool_C".len());
    }

    #[test]
    fn holdback_covers_full_name_awaiting_close_bracket() {
        assert_eq!(holdback_len("hi <tool_call"), "<tool_call".len());
    }

    #[test]
    fn holdback_covers_full_name_with_trailing_space_awaiting_close_bracket() {
        assert_eq!(holdback_len("hi <tool_call "), "<tool_call ".len());
    }

    #[test]
    fn holdback_zero_once_tag_is_closed() {
        // A complete tag is not a "prefix awaiting more" — the caller's confirmed-tag
        // path takes over once `find_next_tag` reports a full match.
        assert_eq!(holdback_len("hi <tool_call>"), 0);
    }

    #[test]
    fn holdback_zero_when_a_non_tag_letter_diverges() {
        assert_eq!(holdback_len("hi <toolbox"), 0);
    }

    #[test]
    fn holdback_handles_multibyte_text_before_the_partial_tag() {
        assert_eq!(holdback_len("xin chào <tool_c"), "<tool_c".len());
    }
}
