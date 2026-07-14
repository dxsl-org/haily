//! Loader for the persisted skill enable/pin admin state (Pipeline Activation phase 5).
//!
//! Enable/pin is written by the cockpit GUI into the generic `meta` prefs table, keyed by
//! skill NAME (`skill.enabled.<name>` / `skill.pinned.<name>` — see `haily-app::cockpit`,
//! which owns the setters `set_skill_enabled`/`pin_skill`). This module owns the SAME key
//! scheme as the single source of truth: `cockpit.rs` imports these constants rather than
//! redeclaring the format strings, so the setter and this reader can never silently drift
//! apart (the phase's named risk: "Name-key mismatch between setter and reader").
//!
//! # Why this loader lives in `haily-kms`, not `haily-app` (deviation from the phase sketch)
//! The phase's architecture sketch places the loader at "the app/caller layer" — but the real
//! injection call site is `haily-core::agent::sub_turn::run_sub_turn`, and `haily-core` cannot
//! depend on `haily-app` (that's the reverse of the actual crate layering: `haily-app` depends
//! on `haily-core`). `haily-kms` already depends on `haily-db` and is depended on by both
//! `haily-core` and `haily-app`, so it is the correct shared home for a DB-backed reader whose
//! output (`SkillGates`) both the injection path (via `KmsHandle::load_skill_gates`) and the
//! cockpit browser could consume.

use crate::skills::SkillGates;
use haily_db::{queries::meta, DbHandle};
use std::collections::HashSet;

/// Preference key prefix for a skill's enabled state — an explicit `"false"` value disables it
/// (absence, or any other value, means enabled).
pub const SKILL_ENABLED_PREFIX: &str = "skill.enabled.";
/// Preference key prefix for a skill's pinned state — an explicit `"true"` value pins it
/// (absence, or any other value, means unpinned).
pub const SKILL_PINNED_PREFIX: &str = "skill.pinned.";

/// Read the current disabled/pinned name sets from the `meta` prefs table. Meant to be called
/// ONCE per injection assembly by the caller that already holds `db` (`KmsHandle::load_skill_gates`).
///
/// A DB read failure yields the default-empty [`SkillGates`] (nothing disabled, nothing
/// pinned) rather than propagating an error — an admin-state read must never break a turn's
/// context assembly (mirrors `haily-app::cockpit::list_skills`'s same fail-open contract).
pub async fn load(db: &DbHandle) -> SkillGates {
    let disabled_prefs = meta::list_by_prefix(db, SKILL_ENABLED_PREFIX).await.unwrap_or_default();
    let pinned_prefs = meta::list_by_prefix(db, SKILL_PINNED_PREFIX).await.unwrap_or_default();

    let disabled: HashSet<String> = disabled_prefs
        .into_iter()
        .filter(|p| p.value == "false")
        .filter_map(|p| p.key.strip_prefix(SKILL_ENABLED_PREFIX).map(str::to_string))
        .collect();
    let pinned: HashSet<String> = pinned_prefs
        .into_iter()
        .filter(|p| p.value == "true")
        .filter_map(|p| p.key.strip_prefix(SKILL_PINNED_PREFIX).map(str::to_string))
        .collect();

    SkillGates::new(disabled, pinned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::queries::meta as meta_queries;

    // Returns the `TempDir` guard alongside the handle — dropping it early deletes the backing
    // file before queries run (see memory note on this exact footgun).
    async fn test_db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = DbHandle::init(&db_path).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn unset_state_yields_default_empty_gates() {
        let (db, _dir) = test_db().await;
        let gates = load(&db).await;
        assert!(!gates.is_disabled("anything"));
        assert!(!gates.is_pinned("anything"));
    }

    #[tokio::test]
    async fn explicit_false_marks_disabled_and_explicit_true_marks_pinned() {
        let (db, _dir) = test_db().await;
        meta_queries::upsert_preference(&db, &format!("{SKILL_ENABLED_PREFIX}foo"), "false", "gui")
            .await
            .unwrap();
        meta_queries::upsert_preference(&db, &format!("{SKILL_PINNED_PREFIX}bar"), "true", "gui")
            .await
            .unwrap();

        let gates = load(&db).await;
        assert!(gates.is_disabled("foo"));
        assert!(!gates.is_disabled("bar"));
        assert!(gates.is_pinned("bar"));
        assert!(!gates.is_pinned("foo"));
    }
}
