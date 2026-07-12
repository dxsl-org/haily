//! 3-tier fetch escalation (Phase 13) — the drop-in port of haily.go's `fetch_strategy.go`.
//!
//! Feature-INDEPENDENT: the bot-signal + human-required keyword tables and the escalation
//! DECISION are pure logic the default build compiles and tests. Only the actual "stealth
//! browser" tier that this decision may point at requires the `browser` cargo feature — when
//! the feature is off, `url_fetch`'s wrapper goes straight to `AWAITING_USER` (see the wrapper
//! in `v1::web::UrlFetchTool`).
//!
//! Escalation ladder (single interactive fetch of ONE url — no batch/multi-target loop):
//!   Tier 1  plain `url_fetch`
//!   Tier 2  on a bot signal (403/429/503 or a challenge keyword) → stealth browser render
//!   Tier 3  on a captcha / login wall in the rendered page → return `AWAITING_USER`, keep the
//!           browser open for the HUMAN to solve (never auto-solved — no CAPTCHA service).

/// HTTP status substrings that indicate bot-blocking. `url_fetch` surfaces 4xx/5xx as the
/// error string (not the result body), so the wrapper checks `err.to_string()` too.
const BLOCK_CODES: &[&str] = &["HTTP 403", "HTTP 429", "HTTP 503"];

/// Challenge-page content keywords — specific enough to avoid false positives on ordinary pages
/// that merely mention "security" in passing.
const CHALLENGE_KEYWORDS: &[&str] = &[
    "checking your browser",
    "enable javascript and cookies",
    "cloudflare",
    "ddos-guard",
    "are you a human",
    "verifying you are human",
    "please wait while we verify",
    "robot check",
    "security check",
];

/// Signals in a rendered page that a HUMAN must act (captcha or login wall). Haily HANDS these
/// back to the user via `AWAITING_USER` — it never integrates a captcha-solving service.
const HUMAN_REQUIRED_KEYWORDS: &[&str] = &[
    "captcha",
    "recaptcha",
    "hcaptcha",
    "turnstile",
    "log in to continue",
    "sign in to see",
    "please log in",
    "login required",
    "verify your identity",
];

/// `true` when a plain-fetch result (or its error message) indicates the request was blocked by
/// a bot-detection system. Case-insensitive on the challenge keywords; the block codes are
/// matched verbatim (they come from a formatted `HTTP {status}` error).
pub fn detect_bot_signal(s: &str) -> bool {
    if BLOCK_CODES.iter().any(|code| s.contains(code)) {
        return true;
    }
    let lower = s.to_lowercase();
    CHALLENGE_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// `true` when a rendered-page result indicates a captcha or login wall requiring user action.
pub fn detect_human_required(rendered: &str) -> bool {
    let lower = rendered.to_lowercase();
    HUMAN_REQUIRED_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// The next action after inspecting a fetch result at a given tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    /// Clean (or a non-bot error) — return the result unchanged.
    Done,
    /// Bot wall detected — escalate to the stealth browser tier (or, without the `browser`
    /// feature, straight to `AwaitingUser`).
    Browser,
    /// Human action required (captcha / login) — return `AWAITING_USER`.
    AwaitingUser,
}

/// Tier-1 decision: inspect the plain `url_fetch` outcome. `check` is the result string, or the
/// error text when `url_fetch` failed (so a `HTTP 403` error is seen).
pub fn after_plain(check: &str) -> Escalation {
    if detect_bot_signal(check) {
        Escalation::Browser
    } else {
        Escalation::Done
    }
}

/// Tier-2 decision: inspect the browser-rendered page.
pub fn after_browser(rendered: &str) -> Escalation {
    if detect_human_required(rendered) {
        Escalation::AwaitingUser
    } else {
        Escalation::Done
    }
}

/// Build the `AWAITING_USER` JSON the wrapper returns to the model. `browser_open` distinguishes
/// the two tier-3 reasons: a rendered captcha/login wall with the browser held open for the
/// human (`true`), versus a bot wall reached with NO browser backend available (`false`, the
/// default-build path) where the user must open the site manually.
pub fn awaiting_user_json(url: &str, browser_open: bool) -> String {
    let url_json = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".to_string());
    let instruction = if browser_open {
        "Browser is open. Complete the CAPTCHA or log in, then reply 'done'."
    } else {
        "This site blocks automated access and no browser backend is available. \
         Open the URL manually to continue, then reply 'done'."
    };
    format!(
        r#"{{"status":"AWAITING_USER","url":{url_json},"browser_open":{browser_open},"instruction":"{instruction}"}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_http_block_codes() {
        assert!(detect_bot_signal("HTTP 403: Forbidden"));
        assert!(detect_bot_signal("request failed: HTTP 429"));
        assert!(detect_bot_signal("HTTP 503 Service Unavailable"));
        assert!(!detect_bot_signal("HTTP 200 OK"));
        assert!(!detect_bot_signal("HTTP 404 Not Found")); // a plain 404 is not a bot wall
    }

    #[test]
    fn detects_challenge_keywords_case_insensitively() {
        assert!(detect_bot_signal("Checking your browser before accessing"));
        assert!(detect_bot_signal("Attention Required! | Cloudflare"));
        assert!(detect_bot_signal("Please enable JavaScript and cookies to continue"));
        assert!(detect_bot_signal("DDOS-GUARD"));
        assert!(!detect_bot_signal("An ordinary page about web security best practices"));
    }

    #[test]
    fn detects_human_required_keywords() {
        assert!(detect_human_required("Please complete the reCAPTCHA"));
        assert!(detect_human_required("Cloudflare Turnstile challenge"));
        assert!(detect_human_required("You must LOG IN TO CONTINUE"));
        assert!(detect_human_required("hCaptcha verification"));
        assert!(!detect_human_required("Welcome back, here is your feed"));
    }

    #[test]
    fn escalation_ladder_plain_to_browser_on_403() {
        // A 403 bot signal at tier 1 escalates to the browser tier.
        assert_eq!(after_plain("HTTP 403: blocked"), Escalation::Browser);
        // A clean page stays done.
        assert_eq!(after_plain("normal article content"), Escalation::Done);
        // A non-bot error (404) stays done — it is not a bot wall.
        assert_eq!(after_plain("HTTP 404 Not Found"), Escalation::Done);
    }

    #[test]
    fn escalation_ladder_browser_to_awaiting_user_on_captcha() {
        assert_eq!(after_browser("please solve the captcha"), Escalation::AwaitingUser);
        assert_eq!(after_browser("rendered page with real content"), Escalation::Done);
    }

    #[test]
    fn awaiting_user_json_is_valid_and_distinguishes_browser_open() {
        let open = awaiting_user_json("https://example.com/x?token=SECRET", true);
        let v: serde_json::Value = serde_json::from_str(&open).expect("valid JSON");
        assert_eq!(v["status"], "AWAITING_USER");
        assert_eq!(v["browser_open"], true);
        assert_eq!(v["url"], "https://example.com/x?token=SECRET");
        assert!(v["instruction"].as_str().unwrap().contains("Browser is open"));

        let closed = awaiting_user_json("https://example.com/", false);
        let v2: serde_json::Value = serde_json::from_str(&closed).expect("valid JSON");
        assert_eq!(v2["browser_open"], false);
        assert!(v2["instruction"].as_str().unwrap().contains("no browser backend"));
    }

    #[test]
    fn awaiting_user_json_escapes_quotes_in_url() {
        // A URL containing a quote must not break the JSON envelope.
        let s = awaiting_user_json(r#"https://x/"; DROP"#, true);
        assert!(serde_json::from_str::<serde_json::Value>(&s).is_ok());
    }
}
