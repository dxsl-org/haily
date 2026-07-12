//! Canonical slash-command registry (Sub-Agent + Skill Architecture phase 11a).
//!
//! ONE source of clean, single-token command names — no `hc-`/`hl-`/`hs-` skill prefix
//! (Haily owns its own command namespace). Every channel PROJECTS this one registry:
//!
//! - GUI / TUI / ACP: use the canonical `name` verbatim (no charset limit).
//! - Telegram (the only constrained channel): its bot-menu charset is `[a-z0-9_]`≤32 and
//!   the menu is a flat single-token list, so only the entries flagged
//!   [`SlashCommand::telegram`] are registered via `setMyCommands`. A multi-token command
//!   (`context-engineering`) is simply NOT on the Telegram menu — no underscore alias, no
//!   added mapping (user decision 2026-07-08); it stays available on the unconstrained
//!   channels. An unregistered `/cmd` on any channel is answered with an unknown-command
//!   hint, never silently swallowed. `/help` is the discovery surface everywhere.
//!
//! Registration is a UX nicety only. An unregistered command still ARRIVES as text — the
//! menu just makes the curated set discoverable without the user knowing it exists.

/// One canonical command. `name` is the bare token WITHOUT a leading slash and without a
/// skill-family prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    /// Canonical token, e.g. `plan`. Lowercase; may contain `-` for multi-word commands
    /// (those are excluded from the Telegram menu — see the module doc).
    pub name: &'static str,
    /// One-line human description shown in menus / `/help`.
    pub description: &'static str,
    /// Whether this command is registered on the Telegram bot menu. Requires a
    /// Telegram-legal name (`[a-z0-9_]`, ≤32, single token — no `-`). Enforced at runtime
    /// by [`telegram_menu`] which re-checks the charset and drops anything illegal so a
    /// future mis-flagged entry can never poison the `setMyCommands` call.
    pub telegram: bool,
}

/// The canonical registry. Ordered as a curated menu (most-used first).
///
/// Single-token entries are the Telegram-eligible subset; the multi-word skills at the end
/// are GUI/TUI/ACP-only. Names mirror the skill families they front (`plan`→`hc-plan`,
/// `fix`→`hc-fix`, …) minus the prefix, plus the local control commands (`undo`, `writes`,
/// `kill`, `help`, `settings`) that already existed as scattered in-band commands and now
/// fold into this one registry.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand { name: "help", description: "List available commands", telegram: true },
    SlashCommand { name: "plan", description: "Plan a feature before coding", telegram: true },
    SlashCommand { name: "brainstorm", description: "Explore options and trade-offs", telegram: true },
    SlashCommand { name: "research", description: "Deep technical research on a topic", telegram: true },
    SlashCommand { name: "review", description: "Production-readiness review of recent changes", telegram: true },
    SlashCommand { name: "fix", description: "Diagnose and fix a bug", telegram: true },
    SlashCommand { name: "scout", description: "Locate code and patterns in the repo", telegram: true },
    SlashCommand { name: "test", description: "Run and validate tests", telegram: true },
    SlashCommand { name: "ship", description: "Review, version, and open a PR", telegram: true },
    SlashCommand { name: "docs", description: "Update project documentation", telegram: true },
    SlashCommand { name: "security", description: "Audit code for vulnerabilities", telegram: true },
    SlashCommand { name: "undo", description: "Undo a recorded action by journal id", telegram: true },
    SlashCommand { name: "writes", description: "Toggle the write kill switch (on/off/status)", telegram: true },
    SlashCommand { name: "kill", description: "Emergency stop: disable all writes now", telegram: true },
    SlashCommand { name: "settings", description: "Open settings / preferences", telegram: true },
    // GUI/TUI/ACP-only (multi-word — not Telegram-legal, no alias by design):
    SlashCommand { name: "context-engineering", description: "Optimize context/agent architecture", telegram: false },
    SlashCommand { name: "mcp-builder", description: "Build or agentize an MCP server", telegram: false },
];

/// The whole registry.
pub fn all() -> &'static [SlashCommand] {
    COMMANDS
}

/// Look up a command by its bare name (no leading slash). `None` if unregistered.
pub fn lookup(name: &str) -> Option<&'static SlashCommand> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// Whether `name` (bare, no slash) is a registered command.
pub fn is_registered(name: &str) -> bool {
    lookup(name).is_some()
}

/// Whether a name is a legal Telegram bot-menu command token: `[a-z0-9_]`, non-empty,
/// ≤32 chars, single token (no `-`). This is the hard charset gate — the `telegram` flag
/// is advisory, this function is authoritative, so a mis-flagged multi-word entry can
/// never reach `setMyCommands`.
fn is_telegram_legal(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The `(name, description)` pairs to register on the Telegram bot menu, in registry order.
/// Filtered by BOTH the `telegram` flag AND the runtime charset check.
pub fn telegram_menu() -> Vec<(String, String)> {
    COMMANDS
        .iter()
        .filter(|c| c.telegram && is_telegram_legal(c.name))
        .map(|c| (c.name.to_string(), c.description.to_string()))
        .collect()
}

/// Parse a leading `/command` out of a message line. Returns the bare command name
/// (lowercased, `@botname` suffix stripped) and the trailing argument text, or `None` if
/// the line is not a slash command. Handles Telegram's `/cmd@botname args` form.
pub fn parse(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix('/')?;
    let (head, args) = match rest.split_once(char::is_whitespace) {
        Some((h, a)) => (h, a.trim()),
        None => (rest, ""),
    };
    // Strip a `@botname` suffix (Telegram appends it in group chats).
    let name = head.split('@').next().unwrap_or(head).to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    Some((name, args.to_string()))
}

/// The `/help` discovery text — the full canonical registry, one `- /name — description`
/// line per command. Shown identically on every channel (the fill for the "must know a
/// skill exists" gap).
pub fn help_text() -> String {
    let mut out = String::from("Available commands:\n");
    for c in COMMANDS {
        out.push_str(&format!("- /{} — {}\n", c.name, c.description));
    }
    out
}

/// The unknown-command hint shown when a `/cmd` does not resolve — never a silent swallow.
pub fn unknown_hint(name: &str) -> String {
    format!("Unknown command /{name}. Send /help to list available commands.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_name_is_prefixless_and_lowercase() {
        for c in COMMANDS {
            assert!(!c.name.starts_with("hc-"), "{} carries a skill prefix", c.name);
            assert!(!c.name.starts_with("hl-"), "{} carries a skill prefix", c.name);
            assert!(!c.name.starts_with("hs-"), "{} carries a skill prefix", c.name);
            assert_eq!(c.name, c.name.to_ascii_lowercase(), "{} must be lowercase", c.name);
            assert!(!c.name.is_empty());
        }
    }

    #[test]
    fn telegram_menu_only_contains_legal_single_token_names() {
        for (name, _) in telegram_menu() {
            assert!(is_telegram_legal(&name), "{name} reached the telegram menu but is not charset-legal");
            assert!(!name.contains('-'), "{name} is multi-word and must not be on the telegram menu");
        }
    }

    #[test]
    fn multiword_commands_are_excluded_from_telegram() {
        let menu = telegram_menu();
        assert!(!menu.iter().any(|(n, _)| n == "context-engineering"));
        assert!(!menu.iter().any(|(n, _)| n == "mcp-builder"));
        // …but they remain in the full registry for the unconstrained channels.
        assert!(is_registered("context-engineering"));
        assert!(is_registered("mcp-builder"));
    }

    #[test]
    fn parse_extracts_name_and_strips_botname() {
        assert_eq!(parse("/help"), Some(("help".into(), "".into())));
        assert_eq!(parse("/undo abc-123"), Some(("undo".into(), "abc-123".into())));
        assert_eq!(parse("/kill@haily_bot"), Some(("kill".into(), "".into())));
        assert_eq!(parse("/writes@haily_bot off"), Some(("writes".into(), "off".into())));
        assert_eq!(parse("PLAN"), None, "no leading slash is not a command");
        assert_eq!(parse("/UnDo X"), Some(("undo".into(), "X".into())), "name is lowercased");
    }

    #[test]
    fn unknown_command_is_recognized_not_registered() {
        assert!(!is_registered("frobnicate"));
        assert!(unknown_hint("frobnicate").contains("/help"));
    }

    #[test]
    fn help_text_lists_the_whole_registry() {
        let help = help_text();
        for c in COMMANDS {
            assert!(help.contains(&format!("/{}", c.name)), "help missing /{}", c.name);
        }
    }
}
