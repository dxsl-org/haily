//! Pure registry construction: unions built-ins + authored + gate-filtered synthesized skills
//! into one name-sorted `Vec<SlashCommand>`, applying slugification and built-in > authored >
//! synthesized precedence (Unified Chat UI phase 2, D1). No I/O — `mod.rs::rebuild` gathers
//! the inputs (DB reads, kit-pack list) and calls straight through to [`build`].
use haily_kms::authored_skills::AuthoredSkillInfo;
use haily_kms::skills::SkillGates;
use serde::Serialize;
use std::collections::HashMap;

/// Which built-in pipeline/control action a slash command maps to. Standalone — NOT a
/// wrapper around `haily_core::RunKind` or `haily_app::trigger::TriggerAction`, which are
/// deliberately non-`Serialize` server-side types (`resolve::resolve` maps this to a
/// `TriggerAction` internally; only this enum ever crosses the `list_slash_commands` wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltInKind {
    /// `/plan <task>` — launch the plan-only pipeline (or prompt for a task if `arg` is empty).
    Plan,
    /// `/code` / `/build <task>` — launch the build-only pipeline (or prompt for a task).
    Build,
    /// Every other built-in in `haily_io::slash::COMMANDS`. `/help`/`/undo`/`/writes`/`/kill`/
    /// `/settings` are intercepted earlier, at the CLI/Telegram adapter layer, before a
    /// `Request` is even constructed — they never reach `resolve()` on those channels. The
    /// skill-family commands (`/review`, `/fix`, `/brainstorm`, …) resolve to an ordinary chat
    /// turn here; the model itself recognizes the intent from the message text.
    PassThrough,
}

/// Which store (if any) a command's underlying skill came from — surfaced to the GUI so the
/// palette can badge authored/synthesized entries distinctly from built-ins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashSource {
    BuiltIn,
    Authored,
    Synthesized,
}

/// What invoking a command actually does. `BuiltIn` reproduces today's `resolve_slash`
/// mapping (see `resolve.rs`); `SkillTurn` carries the skill's OWN name (not the slugified
/// command token) — the injection site re-validates this name against `SkillGates` again at
/// read time (see `haily-kms::KmsHandle::resolve_forced_skill`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum SlashAction {
    BuiltIn(BuiltInKind),
    SkillTurn(String),
}

/// One command in the merged registry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SlashCommand {
    /// Slash-legal token (no leading `/`) — either a built-in's static name or a slugified
    /// skill name; on a precedence collision, the shadowed lower-precedence entry is
    /// reachable instead under a `skill:<slug>` qualified token (never silently dropped).
    pub name: String,
    pub description: String,
    /// Freeform hint for the palette's argument placeholder (e.g. `"<task description>"`).
    pub arg_hint: Option<String>,
    /// A short usage example — populated from a skill's `when_to_use` when available.
    pub example: Option<String>,
    pub action: SlashAction,
    pub source: SlashSource,
}

fn builtin_kind_for(name: &str) -> BuiltInKind {
    match name {
        "plan" => BuiltInKind::Plan,
        "code" | "build" => BuiltInKind::Build,
        _ => BuiltInKind::PassThrough,
    }
}

/// Normalize a skill name into a slash-legal single token: lowercase ASCII letters/digits,
/// runs of whitespace/underscore/hyphen collapsed to a single `-`. Diacritics and other
/// non-ASCII characters are dropped outright (never transliterated) — a name that becomes
/// empty after normalization (e.g. a fully-Vietnamese/diacritic name) returns `None`, and the
/// caller excludes that skill from the registry rather than register a legal-but-garbled slug.
fn slugify(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(lower);
        } else if ch.is_whitespace() || ch == '_' || ch == '-' {
            pending_dash = true;
        }
        // Any other byte (diacritics, punctuation, non-ASCII) is dropped silently.
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Insert `cmd` under `slug`, or — if `slug` is already taken by a higher-or-equal precedence
/// entry — insert it under a `skill:`-qualified alias instead and log the shadowing. Never
/// silently drops a skill (Requirements: "log the shadowed name ... expose it under a
/// `skill:`-qualified token").
fn insert_with_precedence(by_name: &mut HashMap<String, SlashCommand>, slug: String, cmd: SlashCommand) {
    if by_name.contains_key(&slug) {
        let qualified = format!("skill:{slug}");
        tracing::warn!(
            slug = %slug,
            qualified = %qualified,
            source = ?cmd.source,
            "slash command name shadowed by a higher-precedence entry — reachable via qualified token"
        );
        let mut shadowed = cmd;
        shadowed.name = qualified.clone();
        by_name.insert(qualified, shadowed);
        return;
    }
    by_name.insert(slug.clone(), SlashCommand { name: slug, ..cmd });
}

/// Build the merged, name-sorted registry. `io_commands` seeds the built-in entries directly
/// from `haily_io::slash::COMMANDS` (DRY — no re-declaration); `authored`/`synthesized` are
/// filtered by `gates` (disabled names excluded outright) and slugified; precedence on a name
/// collision is built-in > authored > synthesized, with every shadowed name still reachable
/// under a `skill:` prefix rather than dropped.
pub fn build(
    io_commands: &[haily_io::slash::SlashCommand],
    authored: &[AuthoredSkillInfo],
    synthesized: &[haily_db::queries::skills::Skill],
    gates: &SkillGates,
) -> Vec<SlashCommand> {
    let mut by_name: HashMap<String, SlashCommand> = HashMap::new();

    for c in io_commands {
        let arg_hint = matches!(builtin_kind_for(c.name), BuiltInKind::Plan | BuiltInKind::Build)
            .then(|| "<task description>".to_string());
        by_name.insert(
            c.name.to_string(),
            SlashCommand {
                name: c.name.to_string(),
                description: c.description.to_string(),
                arg_hint,
                example: None,
                action: SlashAction::BuiltIn(builtin_kind_for(c.name)),
                source: SlashSource::BuiltIn,
            },
        );
    }

    for skill in authored {
        if gates.is_disabled(&skill.name) {
            continue;
        }
        let Some(slug) = slugify(&skill.name) else {
            tracing::warn!(skill = %skill.name, "authored skill name is not slash-legal — excluded");
            continue;
        };
        insert_with_precedence(
            &mut by_name,
            slug,
            SlashCommand {
                name: String::new(), // set by insert_with_precedence
                description: skill.description.clone(),
                arg_hint: None,
                example: Some(skill.when_to_use.clone()),
                action: SlashAction::SkillTurn(skill.name.clone()),
                source: SlashSource::Authored,
            },
        );
    }

    for skill in synthesized {
        if gates.is_disabled(&skill.name) {
            continue;
        }
        let Some(slug) = slugify(&skill.name) else {
            tracing::warn!(skill = %skill.name, "synthesized skill name is not slash-legal — excluded");
            continue;
        };
        insert_with_precedence(
            &mut by_name,
            slug,
            SlashCommand {
                name: String::new(),
                description: skill.description.clone(),
                arg_hint: None,
                example: None,
                action: SlashAction::SkillTurn(skill.name.clone()),
                source: SlashSource::Synthesized,
            },
        );
    }

    let mut out: Vec<SlashCommand> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::queries::skills::Skill;
    use std::collections::HashSet;

    fn authored(name: &str, description: &str) -> AuthoredSkillInfo {
        AuthoredSkillInfo {
            name: name.to_string(),
            description: description.to_string(),
            when_to_use: format!("use for {name}"),
            kind: "playbook".to_string(),
        }
    }

    fn synth(name: &str, description: &str) -> Skill {
        Skill {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: description.to_string(),
            pattern: description.to_string(),
            steps: "[]".to_string(),
            confidence: 1.0,
            use_count: 0,
            last_used_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            deleted_at: None,
            archived_at: None,
        }
    }

    #[test]
    fn unions_all_three_sources() {
        let out = build(
            haily_io::slash::all(),
            &[authored("db-design", "design a schema")],
            &[synth("weekly-report", "compile the weekly report")],
            &SkillGates::default(),
        );
        assert!(out.iter().any(|c| c.name == "plan" && c.source == SlashSource::BuiltIn));
        assert!(out.iter().any(|c| c.name == "db-design" && c.source == SlashSource::Authored));
        assert!(out.iter().any(|c| c.name == "weekly-report" && c.source == SlashSource::Synthesized));
    }

    #[test]
    fn gate_disabled_skill_is_excluded() {
        let gates = SkillGates::new(HashSet::from(["fix-bug".to_string()]), HashSet::new());
        let out = build(
            haily_io::slash::all(),
            &[authored("fix-bug", "diagnose and fix")],
            &[],
            &gates,
        );
        assert!(!out.iter().any(|c| c.name == "fix-bug"));
    }

    /// Built-in > authored > synthesized: a synthesized skill named after a built-in verb
    /// (`review`) never overrides it, but is still reachable, logged, under `skill:review`.
    #[test]
    fn synthesized_skill_colliding_with_builtin_is_shadowed_but_reachable() {
        let out = build(
            haily_io::slash::all(),
            &[],
            &[synth("review", "my custom review skill")],
            &SkillGates::default(),
        );
        let builtin = out.iter().find(|c| c.name == "review").expect("built-in must survive");
        assert_eq!(builtin.source, SlashSource::BuiltIn);
        let shadowed = out
            .iter()
            .find(|c| c.name == "skill:review")
            .expect("shadowed synthesized skill must be reachable under a qualified token");
        assert_eq!(shadowed.source, SlashSource::Synthesized);
        assert!(matches!(&shadowed.action, SlashAction::SkillTurn(n) if n == "review"));
    }

    /// Authored beats synthesized on a name collision (neither is a built-in here).
    #[test]
    fn authored_beats_synthesized_on_collision() {
        let out = build(
            haily_io::slash::all(),
            &[authored("standup", "authored standup playbook")],
            &[synth("standup", "synthesized standup skill")],
            &SkillGates::default(),
        );
        let winner = out.iter().find(|c| c.name == "standup").expect("one must win");
        assert_eq!(winner.source, SlashSource::Authored);
        assert!(out.iter().any(|c| c.name == "skill:standup" && c.source == SlashSource::Synthesized));
    }

    #[test]
    fn multi_word_names_are_hyphen_joined() {
        assert_eq!(slugify("Context Engineering"), Some("context-engineering".to_string()));
        assert_eq!(slugify("Fix_Bug  Now"), Some("fix-bug-now".to_string()));
    }

    /// A Vietnamese name keeps its ASCII base letters (diacritics are dropped, not the
    /// letter itself) — normalized to a garbled-but-legal, reachable slug, never a crash.
    #[test]
    fn diacritic_name_normalizes_to_a_garbled_but_legal_slug() {
        assert_eq!(slugify("Tối Ưu Hóa"), Some("ti-u-ha".to_string()));
    }

    /// A name with NO ASCII letters/digits at all (no base-letter fallback to keep)
    /// normalizes to empty and is rejected outright — the caller excludes the skill.
    #[test]
    fn name_with_no_ascii_content_is_rejected() {
        assert_eq!(slugify("日本語"), None);
        assert_eq!(slugify("!!!"), None);
    }

    #[test]
    fn skill_with_unslugifiable_name_is_excluded_not_crashed() {
        let out = build(haily_io::slash::all(), &[authored("!!!", "garbage name")], &[], &SkillGates::default());
        assert!(!out.iter().any(|c| c.description == "garbage name"));
    }

    #[test]
    fn output_is_name_sorted() {
        let out = build(haily_io::slash::all(), &[], &[], &SkillGates::default());
        let mut sorted = out.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(out, sorted);
    }
}
