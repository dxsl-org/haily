//! Pure-Rust sentence-boundary chunker for streaming TTS (Mobile Thin-Client plan phase 4).
//! Consumes the assistant's incrementally-streamed `Text` deltas and emits whole, TTS-ready
//! sentences — Android `TextToSpeech`/iOS `AVSpeechSynthesizer` both queue COMPLETE utterance
//! strings, never partial tokens (researcher-02), so this module's entire job is deciding
//! "has a real sentence boundary actually arrived yet" before handing a chunk to the platform
//! layer. No I/O, no platform dependency — the SAME chunker instance is reusable by both
//! Android (this phase) and iOS (P5).
//!
//! **UTF-8 safety note:** every boundary/abbreviation check below scans `buffer.as_bytes()` and
//! only ever tests individual bytes against ASCII punctuation/whitespace/digit values. This is
//! intentionally NOT full UTF-8 decoding — it doesn't need to be, because no ASCII byte value
//! (`< 0x80`) can ever appear as part of a multi-byte UTF-8 sequence (continuation bytes are
//! always `>= 0x80`). So a byte that equals `.`/`!`/`?`/`;`/a digit/whitespace is GUARANTEED to be
//! a genuine standalone ASCII character, never a fragment of a Vietnamese diacritic, and every
//! slice point used below sits immediately after such a byte — always a valid `char` boundary.
use std::collections::HashSet;

/// Vietnamese title/professional abbreviations where a trailing `.` is NOT a sentence boundary
/// (researcher-02) — a distinct problem from English "Mr./Dr." lists. Stored WITHOUT the
/// terminating dot (the token extraction below excludes it); `PGS.TS` covers the chained form
/// (`PGS.TS. Nguyễn ...`) because chained titles have no internal whitespace, so the token
/// scanned back to the last whitespace boundary is the whole chain, not just the last segment.
const VN_ABBREVIATIONS: &[&str] = &[
    "ThS", "TS", "BS", "GS", "PGS", "PGS.TS", "TS.BS", "Tr", "v.v", "TP", "Th.S",
];

/// Below this length a completed-looking sentence is treated as too short to stand alone
/// (researcher-02: "~10 chars") and is merged into whatever follows instead of being flushed.
const MIN_CHUNK_LEN: usize = 10;

/// Above this many chars the buffer is force-flushed at the last word boundary even with no
/// sentence terminator in sight — a terminator-less run-on (list output, code, a stream that
/// never ends in punctuation) must not grow the buffer unbounded nor defeat sentence-by-sentence
/// streaming by arriving as one giant blob at `flush`.
const MAX_CHUNK_LEN: usize = 400;

/// Accumulates streamed text and yields whole sentences at real boundaries. See the module doc
/// for the terminator/abbreviation/decimal rules and the UTF-8 safety invariant.
pub struct TtsChunker {
    buffer: String,
    denylist: HashSet<&'static str>,
}

impl Default for TtsChunker {
    fn default() -> Self {
        Self::new()
    }
}

impl TtsChunker {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            denylist: VN_ABBREVIATIONS.iter().copied().collect(),
        }
    }

    /// Feeds a streamed text delta, returning zero or more newly-completed sentences in order.
    /// Deterministic and side-effect-free beyond the internal buffer — repeated calls with the
    /// same total input always yield the same split points regardless of how it was chunked into
    /// deltas (a boundary is only ever accepted once the character AFTER the terminator is known,
    /// so a terminator that lands on a delta's last byte correctly waits for the next `push`).
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);
        let mut out = Vec::new();
        loop {
            let boundary = match self.find_boundary() {
                Some(b) => Some(b),
                None => self.overflow_boundary(),
            };
            let Some(boundary) = boundary else { break };
            let sentence: String = self.buffer.drain(..boundary).collect();
            let trimmed = sentence.trim().to_string();
            if !trimmed.is_empty() {
                out.push(trimmed);
            }
        }
        out
    }

    /// Forced cut point for a buffer that exceeded [`MAX_CHUNK_LEN`] without a real sentence
    /// boundary: the byte offset just after the LAST ASCII whitespace in the buffer (a soft word
    /// boundary — always a valid `char` boundary, see the module's UTF-8 note), or the end of the
    /// last full `char` within the limit when the run-on contains no whitespace at all.
    fn overflow_boundary(&self) -> Option<usize> {
        // Byte offset of the char AFTER the MAX_CHUNK_LEN-th one — `None` means the buffer is
        // still within the limit.
        let limit_idx = self.buffer.char_indices().nth(MAX_CHUNK_LEN)?.0;
        let head = &self.buffer.as_bytes()[..limit_idx];
        match head.iter().rposition(|b| b.is_ascii_whitespace()) {
            // Cut just after the last word boundary WITHIN the limit, so the emitted chunk never
            // exceeds MAX_CHUNK_LEN chars.
            Some(ws) if ws > 0 => Some(ws + 1),
            // No whitespace in the whole head — hard-cut at the limit (a char boundary).
            _ => Some(limit_idx),
        }
    }

    /// Stream ended (`ResponseChunk::Complete`) — emits whatever remains, even under
    /// `MIN_CHUNK_LEN` and even with no terminator at all (researcher-02: "always flush any
    /// remainder when the LLM stream ends"). Returns `None` if nothing was left to say.
    pub fn flush(&mut self) -> Option<String> {
        let remainder = self.buffer.trim().to_string();
        self.buffer.clear();
        if remainder.is_empty() {
            None
        } else {
            Some(remainder)
        }
    }

    /// Discards any buffered partial without emitting it — the `ResponseChunk::Error` contract
    /// (haily-types: "discard the partial buffer"): a failed/cancelled turn's tail must never be
    /// glued onto (and spoken before) the NEXT turn's text.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Byte offset (end-exclusive) of the first ACCEPTED sentence boundary in `self.buffer`, or
    /// `None` if nothing qualifies yet. A candidate is a `.`/`!`/`?`/`;` immediately followed by
    /// whitespace; a candidate is then rejected (scanning continues past it, extending the
    /// eventual sentence) if it's a `.` preceded by a digit (decimal/thousands separator, e.g.
    /// `1.000.000`), a `.` ending a known VN abbreviation token, or if the resulting trimmed
    /// sentence would be shorter than [`MIN_CHUNK_LEN`] — in all three cases the fragment merges
    /// into whatever comes next rather than being flushed (and immediately re-matched) alone.
    fn find_boundary(&self) -> Option<usize> {
        let bytes = self.buffer.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i] as char;
            if matches!(c, '.' | '!' | '?' | ';') {
                let next_is_space = bytes
                    .get(i + 1)
                    .map(|b| b.is_ascii_whitespace())
                    .unwrap_or(false);
                if next_is_space {
                    let preceded_by_digit = i > 0 && (bytes[i - 1] as char).is_ascii_digit();
                    let is_abbreviation = c == '.' && self.ends_in_abbreviation(i);
                    if !preceded_by_digit && !is_abbreviation {
                        // `chars().count()`, NOT byte length — Vietnamese diacritics are 2-3
                        // bytes each, so byte length would over-count short phrases as "long
                        // enough" and defeat the merge-short-fragments rule for exactly the
                        // language this chunker targets.
                        let candidate_len = self.buffer[..=i].trim().chars().count();
                        if candidate_len >= MIN_CHUNK_LEN {
                            return Some(i + 1);
                        }
                        // Too short to stand alone — keep scanning; the next accepted boundary
                        // (or `flush` on stream end) will carry this fragment along with it.
                    }
                }
            }
            i += 1;
        }
        None
    }

    /// Whether the token ending at `dot_index` (scanned back to the nearest whitespace, or the
    /// buffer start) is a known VN title/abbreviation — see [`VN_ABBREVIATIONS`].
    fn ends_in_abbreviation(&self, dot_index: usize) -> bool {
        let bytes = self.buffer.as_bytes();
        let mut start = dot_index;
        // MUST be `is_ascii_whitespace`, never `(byte as char).is_whitespace()`: 0xA0/0x85 are
        // whitespace as chars (NBSP/NEL) but also valid UTF-8 CONTINUATION bytes, so the char
        // version can stop this backward scan mid-codepoint and the slice below then panics on a
        // non-char-boundary (reproduced with U+20000, first byte sequence F0 A0 80 80).
        while start > 0 && !bytes[start - 1].is_ascii_whitespace() {
            start -= 1;
        }
        let token = &self.buffer[start..dot_index];
        self.denylist.contains(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_chunker_starts_empty() {
        let mut c = TtsChunker::new();
        assert!(c.flush().is_none());
    }

    // Every fixture below that expects its FINAL sentence back from a single `push()` call ends
    // with a trailing space — a boundary is only accepted once the character AFTER the
    // terminator is known (see `find_boundary`'s doc comment), so a terminator sitting on the
    // very last byte of the buffer correctly waits for more input. This mirrors real LLM
    // streaming, which virtually always emits a trailing space/newline token before `Complete`;
    // the no-trailing-space case is covered separately by `flush_emits_a_remainder_...` and
    // `a_terminator_split_across_two_pushes_...` below.

    #[test]
    fn a_single_push_with_two_real_sentences_emits_both() {
        let mut c = TtsChunker::new();
        let out = c.push("Bạn khỏe không? Tôi khỏe, cảm ơn bạn! ");
        assert_eq!(out, vec!["Bạn khỏe không?", "Tôi khỏe, cảm ơn bạn!"]);
        assert!(c.flush().is_none(), "everything should already be drained");
    }

    #[test]
    fn a_short_greeting_merges_into_the_following_sentence() {
        // "Xin chào." alone is under MIN_CHUNK_LEN — must not be flushed as its own utterance.
        let mut c = TtsChunker::new();
        let out = c.push("Xin chào. Tôi có thể giúp gì cho bạn? ");
        assert_eq!(out, vec!["Xin chào. Tôi có thể giúp gì cho bạn?"]);
    }

    #[test]
    fn several_consecutive_short_interjections_merge_until_the_threshold_then_flush() {
        // Regression guard: an earlier draft's "glue short fragment back onto the buffer" design
        // re-created the identical short-prefix pattern on every loop iteration and hung. Each of
        // "Ừ."/"À." alone is short, but the CUMULATIVE prefix "Ừ. À. Được." already clears
        // MIN_CHUNK_LEN once "Được." lands — that's the correct point to flush, not "merge every
        // fragment all the way to the very last terminator in the input" (a stronger, unneeded
        // guarantee this chunker doesn't make).
        let mut c = TtsChunker::new();
        let out = c.push("Ừ. À. Được. Tôi hiểu rồi, cảm ơn bạn nhé. ");
        assert_eq!(out, vec!["Ừ. À. Được.", "Tôi hiểu rồi, cảm ơn bạn nhé."]);
    }

    #[test]
    fn pgs_ts_chained_abbreviation_does_not_split_mid_sentence() {
        let mut c = TtsChunker::new();
        let out =
            c.push("Theo PGS.TS. Nguyễn Văn A, đây là kết quả nghiên cứu quan trọng vừa công bố. ");
        assert_eq!(
            out,
            vec!["Theo PGS.TS. Nguyễn Văn A, đây là kết quả nghiên cứu quan trọng vừa công bố."]
        );
    }

    #[test]
    fn ths_single_abbreviation_does_not_split_mid_sentence() {
        let mut c = TtsChunker::new();
        let out = c.push("Chị ThS. Lan phụ trách lớp học phần này trong học kỳ tới. ");
        assert_eq!(
            out,
            vec!["Chị ThS. Lan phụ trách lớp học phần này trong học kỳ tới."]
        );
    }

    #[test]
    fn v_v_abbreviation_does_not_split_mid_sentence() {
        let mut c = TtsChunker::new();
        let out = c.push("Cần chuẩn bị hồ sơ, ảnh, chứng minh thư, v.v. trước khi nộp đơn. ");
        assert_eq!(
            out,
            vec!["Cần chuẩn bị hồ sơ, ảnh, chứng minh thư, v.v. trước khi nộp đơn."]
        );
    }

    #[test]
    fn ellipsis_followed_by_whitespace_is_treated_as_a_boundary() {
        let mut c = TtsChunker::new();
        let out = c.push("Chờ chút một lát... để tôi kiểm tra lại thông tin đã nhé. ");
        assert_eq!(
            out,
            vec![
                "Chờ chút một lát...",
                "để tôi kiểm tra lại thông tin đã nhé."
            ]
        );
    }

    #[test]
    fn thousands_separator_dots_are_never_boundaries() {
        let mut c = TtsChunker::new();
        let out = c.push("Số tiền là 1.000.000 đồng, xin cảm ơn quý khách của chúng tôi. ");
        assert_eq!(
            out,
            vec!["Số tiền là 1.000.000 đồng, xin cảm ơn quý khách của chúng tôi."]
        );
    }

    #[test]
    fn digit_before_terminator_is_never_a_boundary_even_with_trailing_space() {
        // Simulates a decimal number whose "." happened to land next to whitespace (e.g. a
        // stream-delta artifact like "2. 0") — the digit-precedes-dot rule must win regardless.
        let mut c = TtsChunker::new();
        let out = c.push("Phiên bản 2. 0 sắp ra mắt trong tháng tới theo kế hoạch. ");
        assert_eq!(
            out,
            vec!["Phiên bản 2. 0 sắp ra mắt trong tháng tới theo kế hoạch."]
        );
    }

    #[test]
    fn a_terminator_split_across_two_pushes_is_recognized_once_the_next_delta_confirms_it() {
        let mut c = TtsChunker::new();
        // The first delta ends exactly on the period — nothing is known about what follows yet.
        let first = c.push("Đây là câu đầu tiên.");
        assert!(first.is_empty(), "must wait to see what follows the '.'");
        let second = c.push(" Đây là câu thứ hai, đủ dài để đứng riêng. ");
        assert_eq!(
            second,
            vec![
                "Đây là câu đầu tiên.",
                "Đây là câu thứ hai, đủ dài để đứng riêng."
            ]
        );
    }

    #[test]
    fn flush_emits_a_remainder_with_no_terminator_at_all() {
        let mut c = TtsChunker::new();
        let out = c.push("Xin chào, tôi đang xử lý yêu cầu của bạn");
        assert!(out.is_empty());
        assert_eq!(
            c.flush(),
            Some("Xin chào, tôi đang xử lý yêu cầu của bạn".to_string())
        );
    }

    #[test]
    fn flush_on_an_already_drained_buffer_is_none() {
        let mut c = TtsChunker::new();
        let out = c.push("Câu hoàn chỉnh và đủ dài để đứng một mình. ");
        assert!(!out.is_empty());
        assert!(c.flush().is_none());
    }

    #[test]
    fn multibyte_char_whose_continuation_bytes_look_like_whitespace_does_not_panic() {
        // Regression (review HIGH): U+20000 encodes as F0 A0 80 80 — 0xA0 is whitespace as a
        // char (NBSP) but here is a CONTINUATION byte; the abbreviation back-scan must not stop
        // mid-codepoint and panic on the token slice.
        let mut c = TtsChunker::new();
        let _ = c.push("𠀀. ");
        let _ = c.push("Chữ Nôm 𠀀 nằm giữa câu này và câu vẫn được tách đúng chỗ. ");
        // Reaching here without panic is the assertion; flush drains whatever remains.
        let _ = c.flush();
    }

    #[test]
    fn a_terminator_less_run_on_is_force_flushed_at_a_word_boundary() {
        let mut c = TtsChunker::new();
        let word = "từ ";
        let long_input: String = word.repeat(150); // 450 chars, no terminator anywhere
        let out = c.push(&long_input);
        assert!(
            !out.is_empty(),
            "overflow must force a flush instead of buffering unbounded"
        );
        for chunk in &out {
            assert!(!chunk.ends_with(char::is_whitespace));
            assert!(chunk.chars().count() <= MAX_CHUNK_LEN + word.chars().count());
        }
        assert!(c.buffer.chars().count() <= MAX_CHUNK_LEN);
    }

    #[test]
    fn a_whitespace_free_run_on_hard_cuts_on_a_char_boundary() {
        let mut c = TtsChunker::new();
        let long_input: String = "ầ".repeat(MAX_CHUNK_LEN + 50); // multibyte, zero whitespace
        let out = c.push(&long_input);
        assert!(!out.is_empty());
        assert_eq!(out[0].chars().count(), MAX_CHUNK_LEN);
        let _ = c.flush();
    }

    #[test]
    fn clear_discards_a_buffered_partial_without_emitting_it() {
        let mut c = TtsChunker::new();
        let out = c.push("Câu này chưa kết thúc và sẽ bị hủy giữa chừng");
        assert!(out.is_empty());
        c.clear();
        assert!(c.flush().is_none(), "cleared partial must be gone");
        let next = c.push("Lượt sau bắt đầu sạch sẽ, không dính phần cũ nhé bạn. ");
        assert_eq!(
            next,
            vec!["Lượt sau bắt đầu sạch sẽ, không dính phần cũ nhé bạn."]
        );
    }

    #[test]
    fn semicolon_terminates_a_sentence() {
        let mut c = TtsChunker::new();
        let out = c.push("Việc này rất quan trọng; hãy làm cẩn thận nhé bạn tôi ơi. ");
        assert_eq!(
            out,
            vec![
                "Việc này rất quan trọng;",
                "hãy làm cẩn thận nhé bạn tôi ơi."
            ]
        );
    }
}
