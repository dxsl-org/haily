//! Human-behavior math (Phase 13) — the drop-in port of haily.go's `browser_human.go`.
//!
//! Feature-INDEPENDENT: these are pure functions with the randomness INJECTED as a value in
//! `[0, 1)`, so the bounds are unit-testable without a live browser. The `browser`-gated driver
//! (`manager.rs`) supplies `rand::random::<f64>()` at call time and dispatches the resulting
//! keystroke/mouse events over CDP.
//!
//! Purpose: defeat behavioral fingerprint classifiers that flag teleport-then-click and
//! zero-variance keystroke timing — for the OWNER's own single interactive session. Nothing
//! here loops over multiple targets or accounts.

/// Per-rune typing delay window (ms): 30–119 ms, matching the prior humanTypeRunes.
pub const TYPE_DELAY_MIN_MS: u64 = 30;
pub const TYPE_DELAY_MAX_MS: u64 = 119;

/// Probability of an injected typo (then backspace + correct) per rune — 5%.
pub const TYPO_RATE: f64 = 0.05;

/// Mouse-move sample count window for a Bézier approach: 8–15 samples.
pub const MOUSE_SAMPLES_MIN: u32 = 8;
pub const MOUSE_SAMPLES_MAX: u32 = 15;

/// Map a random value `r` in `[0, 1)` to a per-rune type delay in `[30, 119]` ms.
pub fn type_delay_ms(r: f64) -> u64 {
    let span = (TYPE_DELAY_MAX_MS - TYPE_DELAY_MIN_MS) as f64; // 89
    TYPE_DELAY_MIN_MS + (r.clamp(0.0, 1.0) * span) as u64
}

/// `true` when a random value `r` in `[0, 1)` should trigger an injected typo (5% rate).
pub fn should_typo(r: f64) -> bool {
    r < TYPO_RATE
}

/// Map a random value `r` in `[0, 1)` to a Bézier mouse-move sample count in `[8, 15]`.
pub fn mouse_samples(r: f64) -> u32 {
    let span = MOUSE_SAMPLES_MAX - MOUSE_SAMPLES_MIN; // 7
    // `.min(MAX)` guards the r == 1.0 boundary: `1.0 * (span+1)` truncates to `span+1`, which
    // would otherwise land one past MAX (16). Every r in [0, 1] now maps into [MIN, MAX].
    (MOUSE_SAMPLES_MIN + (r.clamp(0.0, 1.0) * (span as f64 + 1.0)) as u32).min(MOUSE_SAMPLES_MAX)
}

/// `true` for a standard printable ASCII rune (0x20–0x7E) — these dispatch as real keydown/keyup
/// CDP key events; non-ASCII (Vietnamese, CJK, emoji) must use `Input.insertText` instead
/// (multi-byte Unicode has no single keymap entry).
pub fn is_ascii_printable(c: char) -> bool {
    ('\u{20}'..='\u{7E}').contains(&c)
}

/// Quadratic Bézier control point offset perpendicular to the start→end vector, ±15% of the
/// distance capped at 200 px, scaled by `jitter_factor` in `[-1, 1]`. Gives a natural curved
/// approach instead of a straight teleport. Returns the control point `(cx, cy)`.
pub fn bezier_control(start: (f64, f64), end: (f64, f64), jitter_factor: f64) -> (f64, f64) {
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let dist = dx.hypot(dy);
    let (jx, jy) = if dist > 0.0 {
        let jitter = (dist * 0.15).min(200.0) * jitter_factor.clamp(-1.0, 1.0);
        (-dy / dist * jitter, dx / dist * jitter)
    } else {
        (0.0, 0.0)
    };
    (start.0 + dx * 0.5 + jx, start.1 + dy * 0.5 + jy)
}

/// Evaluate a quadratic Bézier curve at parameter `t` in `[0, 1]` given the start, control, and
/// end points. `t=0` is the start, `t=1` is the end.
pub fn bezier_point(
    start: (f64, f64),
    ctrl: (f64, f64),
    end: (f64, f64),
    t: f64,
) -> (f64, f64) {
    let t = t.clamp(0.0, 1.0);
    let omt = 1.0 - t;
    let x = omt * omt * start.0 + 2.0 * omt * t * ctrl.0 + t * t * end.0;
    let y = omt * omt * start.1 + 2.0 * omt * t * ctrl.1 + t * t * end.1;
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_delay_stays_within_the_ported_window() {
        assert_eq!(type_delay_ms(0.0), TYPE_DELAY_MIN_MS);
        // r just under 1.0 → max minus at most 1 (integer truncation of 89 * r).
        let hi = type_delay_ms(0.999);
        assert!((TYPE_DELAY_MIN_MS..=TYPE_DELAY_MAX_MS).contains(&hi), "delay {hi} out of window");
        // Sweep the whole range — never escapes the bounds.
        for i in 0..=1000 {
            let d = type_delay_ms(i as f64 / 1000.0);
            assert!((TYPE_DELAY_MIN_MS..=TYPE_DELAY_MAX_MS).contains(&d));
        }
    }

    #[test]
    fn typo_rate_is_five_percent() {
        assert!(should_typo(0.0));
        assert!(should_typo(0.049));
        assert!(!should_typo(0.05));
        assert!(!should_typo(0.5));
    }

    #[test]
    fn mouse_samples_stay_within_8_to_15() {
        assert_eq!(mouse_samples(0.0), MOUSE_SAMPLES_MIN);
        for i in 0..=1000 {
            let n = mouse_samples(i as f64 / 1000.0);
            assert!((MOUSE_SAMPLES_MIN..=MOUSE_SAMPLES_MAX).contains(&n), "samples {n} out of window");
        }
    }

    #[test]
    fn ascii_printable_classification() {
        assert!(is_ascii_printable('a'));
        assert!(is_ascii_printable(' '));
        assert!(is_ascii_printable('~'));
        assert!(!is_ascii_printable('\n'));
        assert!(!is_ascii_printable('ế')); // Vietnamese → InsertText path
        assert!(!is_ascii_printable('中')); // CJK → InsertText path
        assert!(!is_ascii_printable('😀')); // emoji → InsertText path
    }

    #[test]
    fn bezier_endpoints_are_exact() {
        let start = (0.0, 0.0);
        let end = (100.0, 50.0);
        let ctrl = bezier_control(start, end, 0.5);
        let p0 = bezier_point(start, ctrl, end, 0.0);
        let p1 = bezier_point(start, ctrl, end, 1.0);
        assert!((p0.0 - start.0).abs() < 1e-9 && (p0.1 - start.1).abs() < 1e-9);
        assert!((p1.0 - end.0).abs() < 1e-9 && (p1.1 - end.1).abs() < 1e-9);
    }

    #[test]
    fn bezier_control_offset_is_bounded_and_perpendicular() {
        let start = (0.0, 0.0);
        let end = (1000.0, 0.0); // horizontal → perpendicular offset is purely vertical
        // Max jitter = min(dist*0.15, 200) = min(150, 200) = 150 at factor 1.0.
        let ctrl = bezier_control(start, end, 1.0);
        assert!((ctrl.0 - 500.0).abs() < 1e-9, "midpoint x should be halfway");
        assert!((ctrl.1.abs() - 150.0).abs() < 1e-6, "perpendicular offset should be 150px");
        // Zero jitter → straight-line midpoint.
        let straight = bezier_control(start, end, 0.0);
        assert!((straight.1).abs() < 1e-9);
    }

    #[test]
    fn bezier_control_caps_offset_at_200px() {
        let start = (0.0, 0.0);
        let end = (10000.0, 0.0); // dist*0.15 = 1500, capped to 200
        let ctrl = bezier_control(start, end, 1.0);
        assert!((ctrl.1.abs() - 200.0).abs() < 1e-6, "offset must cap at 200px");
    }

    #[test]
    fn bezier_control_handles_zero_distance() {
        let ctrl = bezier_control((5.0, 5.0), (5.0, 5.0), 1.0);
        assert_eq!(ctrl, (5.0, 5.0));
    }
}
