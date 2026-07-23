//! Data-driven slash-command registry (Unified Chat UI phase 2, D1) — unions the static
//! built-in commands (`haily-io::slash::COMMANDS`) with authored (kit-pack) and
//! gate-filtered synthesized skills into one `Vec<SlashCommand>`, serving both the GUI's
//! `list_slash_commands` command and `trigger::resolve`'s per-request slash routing.
//!
//! Rebuild is LAZY, not push-driven (P02↔P08 interop contract, locked): `ensure_fresh` polls
//! `AuthoredRegistry::version()` and rebuilds only when it has moved since the last build —
//! phase 08's skill editor calls only the existing `AuthoredRegistry::reload()`, adding no
//! new hook. A rebuild also re-reads `active_skills`/`SkillGates` from the DB, so a
//! synthesized skill's enable/pin/decay-archival state is picked up on the same poll.
mod build;
pub mod resolve;

pub use build::{build, BuiltInKind, SlashAction, SlashCommand, SlashSource};

use haily_db::{queries::skills as db_skills, DbHandle};
use haily_kms::KmsHandle;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Sentinel meaning "never built" — distinct from a real `AuthoredRegistry::version()` value
/// (which starts at 0 for an empty/no-kit-pack registry), so `ensure_fresh` cannot mistake an
/// UNBUILT registry for one that is merely up to date with a version-0 kit-pack.
const NEVER_BUILT: u64 = u64::MAX;

/// Hot-swappable snapshot of the merged registry. Cheap to clone (`Arc`) and share across the
/// dispatch loop's per-request tasks and the Tauri command layer.
pub struct SlashRegistry {
    snapshot: RwLock<Arc<Vec<SlashCommand>>>,
    /// The `AuthoredRegistry::version()` value the current snapshot was built from.
    built_authored_version: AtomicU64,
}

impl Default for SlashRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SlashRegistry {
    pub fn new() -> Self {
        SlashRegistry {
            snapshot: RwLock::new(Arc::new(Vec::new())),
            built_authored_version: AtomicU64::new(NEVER_BUILT),
        }
    }

    /// Rebuild the merged registry from the current authored/synthesized/gate state and swap
    /// it in. A DB read failure for the synthesized side degrades to "no synthesized skills
    /// this build" (logged) rather than failing the whole rebuild — a transient DB hiccup
    /// must never crash slash-command resolution.
    ///
    /// `authored_version()` is captured BEFORE reading `authored_skills_list()` (not after) —
    /// a `reload()` landing in between must not be swallowed: storing the version read
    /// AFTER the data would record a version newer than what was actually built, so the next
    /// `ensure_fresh` call sees a match and skips a rebuild that reload genuinely warranted.
    /// Capturing first means the recorded version is, if anything, stale-by-one relative to a
    /// concurrent reload — `ensure_fresh` simply rebuilds again next poll, which is harmless.
    pub async fn rebuild(&self, kms: &KmsHandle, db: &DbHandle) {
        let version_at_read = kms.authored_version();
        let gates = kms.load_skill_gates().await;
        let authored = kms.authored_skills_list();
        let synthesized = db_skills::active_skills(db).await.unwrap_or_else(|e| {
            tracing::warn!("slash registry rebuild: active_skills read failed: {e:#}");
            Vec::new()
        });

        let commands = build(haily_io::slash::all(), &authored, &synthesized, &gates);
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(commands);
        self.built_authored_version
            .store(version_at_read, Ordering::SeqCst);
    }

    /// Rebuild only if the authored-skill kit-pack has changed since the last build (lazy
    /// polling contract, module doc). Cheap no-op on the common "nothing changed" path.
    pub async fn ensure_fresh(&self, kms: &KmsHandle, db: &DbHandle) {
        if kms.authored_version() != self.built_authored_version.load(Ordering::Acquire) {
            self.rebuild(kms, db).await;
        }
    }

    /// Look up one command by its registered (already-slugified) name.
    pub fn lookup(&self, name: &str) -> Option<SlashCommand> {
        self.snap().iter().find(|c| c.name == name).cloned()
    }

    /// The full current registry, name-sorted — for `list_slash_commands`.
    pub fn snapshot(&self) -> Vec<SlashCommand> {
        (*self.snap()).clone()
    }

    fn snap(&self) -> Arc<Vec<SlashCommand>> {
        Arc::clone(&self.snapshot.read().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn never_built_registry_is_empty_until_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let db = haily_db::DbHandle::init(&dir.path().join("t.db"))
            .await
            .unwrap();
        let kms = KmsHandle::init(db.clone(), dir.path()).await.unwrap();
        let registry = SlashRegistry::new();
        assert!(registry.snapshot().is_empty());

        registry.ensure_fresh(&kms, &db).await;
        assert!(
            !registry.snapshot().is_empty(),
            "ensure_fresh must build on first call even though version() starts at 0"
        );
        assert!(registry.lookup("plan").is_some());
    }

    #[tokio::test]
    async fn ensure_fresh_is_a_noop_when_kit_pack_version_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let db = haily_db::DbHandle::init(&dir.path().join("t.db"))
            .await
            .unwrap();
        let kms = KmsHandle::init(db.clone(), dir.path()).await.unwrap();
        let registry = SlashRegistry::new();
        registry.rebuild(&kms, &db).await;

        // Insert a synthesized skill directly — a rebuild would pick it up, but
        // ensure_fresh must skip rebuilding because the authored version hasn't moved.
        db_skills::insert_skill(&db, "new-skill", "desc", "pattern", "[]")
            .await
            .unwrap();
        registry.ensure_fresh(&kms, &db).await;
        assert!(
            registry.lookup("new-skill").is_none(),
            "ensure_fresh must not rebuild when AuthoredRegistry::version() is unchanged"
        );
    }
}
