//! Edit/revert/promote/archive orchestration (Unified Chat UI phase 8, D4) — the async glue
//! between the pure markdown mapping (`super::markdown`), the atomic kit-pack write
//! (`crate::kit_pack`), and the shared version-history table
//! (`haily_db::queries::skill_versions`).

use super::guard::validate_skill_name;
use super::markdown::{parse_markdown, render_markdown};
use super::{SkillDetail, SkillDraft, SkillEditKind, MAX_FIELD_BYTES};
use crate::authored_skills::{AuthoredSkill, SkillKind};
use crate::kit_pack;
use crate::KmsHandle;
use anyhow::{anyhow, bail, Result};
use haily_db::queries::{skill_versions, skills as db_skills};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use tokio::sync::Mutex as AsyncMutex;

/// Serializes the on-disk kit-pack write path (review MED-2): `write_skill_atomic` reads the
/// WHOLE `manifest.json`, adds/updates one entry, and renames the file back — two concurrent
/// `edit_skill`/`revert_skill`/`promote_to_authored` calls on DIFFERENT skills would otherwise
/// race that read-modify-write, and the second writer's commit (built from a manifest snapshot
/// that predates the first writer's rename) would silently clobber the first writer's hash
/// update, making its just-written file mismatch at next boot. One process-wide lock is
/// sufficient — there is exactly one kit-pack directory per process (no multi-tenant kit-pack
/// scenario exists), so this is equivalent in practice to a per-registry lock without needing a
/// new `KmsHandle`/`AuthoredRegistry` field.
fn authored_write_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Reject an oversized structured field before it reaches a render/write path (Security
/// Considerations: "cap content size; validate the 4 fields").
fn validate_draft(draft: &SkillDraft) -> Result<()> {
    for (label, field) in [
        ("procedure", &draft.procedure),
        ("success_conditions", &draft.success_conditions),
        ("forbidden_actions", &draft.forbidden_actions),
        ("required_from_user", &draft.required_from_user),
    ] {
        if field.len() > MAX_FIELD_BYTES {
            bail!("field '{label}' exceeds the {MAX_FIELD_BYTES}-byte cap");
        }
    }
    Ok(())
}

/// Fetch the current live content of `name`/`kind`, mapped into the structured draft shape.
///
/// # Errors
/// Returns an error if the skill is unknown for the given kind.
pub async fn get_skill_detail(kms: &KmsHandle, name: &str, kind: SkillEditKind) -> Result<SkillDetail> {
    let body = match kind {
        SkillEditKind::Authored => {
            kms.authored_get(name).ok_or_else(|| anyhow!("unknown authored skill '{name}'"))?.body
        }
        SkillEditKind::Synthesized => {
            db_skills::get_skill_by_name(&kms.db, name)
                .await?
                .ok_or_else(|| anyhow!("unknown synthesized skill '{name}'"))?
                .description
        }
    };
    Ok(SkillDetail { name: name.to_string(), kind: kind.as_str().to_string(), draft: parse_markdown(&body) })
}

/// Every recorded version of `name`, newest first — spans both kinds, since `skill_versions` is
/// the one history mechanism for both (D4).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_versions(kms: &KmsHandle, name: &str) -> Result<Vec<skill_versions::SkillVersion>> {
    skill_versions::list_versions(&kms.db, name).await
}

/// Snapshot the CURRENT live content of `name`/`kind` into `skill_versions` before any mutation.
/// One mechanism gives both explicit version history AND crash safety: a crash mid-edit leaves
/// the pre-edit content recoverable (the CRITICAL manifest-atomicity finding —
/// `kit_pack::load_with_versions_fallback`) without a separate "record what was just saved"
/// step, since what THIS snapshot captures is exactly what the previous save (or the original
/// kit-pack file) produced. A no-op (`Ok(())`) when there is nothing yet to snapshot (a
/// brand-new promote target).
async fn snapshot_current(kms: &KmsHandle, name: &str, kind: SkillEditKind, note: Option<&str>) -> Result<()> {
    let content = match kind {
        SkillEditKind::Authored => match kms.authored_get(name) {
            Some(skill) => skill.to_markdown(),
            None => return Ok(()),
        },
        SkillEditKind::Synthesized => match db_skills::get_skill_by_name(&kms.db, name).await? {
            Some(row) => row.description,
            None => return Ok(()),
        },
    };
    let hash = sha256_hex(content.as_bytes());
    skill_versions::insert_version(&kms.db, name, kind.as_str(), &content, &hash, note).await?;
    Ok(())
}

/// Edit an existing skill's structured fields (D4). Snapshots the pre-edit content into
/// `skill_versions` FIRST (see `snapshot_current`), then writes the new content through the
/// atomic path for `kind`, returning the full saved content.
///
/// The filename-legality guard (`validate_skill_name`) applies ONLY to the authored path
/// (review LOW: it is a filesystem-safety allowlist, and a synthesized `kms_skills` row is
/// never a path — gating it here too would let a skill with an exotic LLM-authored name be
/// opened via `get_skill_detail` but never saved). `write_authored_full` re-checks it anyway
/// for defense in depth on every authored write (edit/revert/promote).
///
/// # Errors
/// Returns an error if `kind == Authored` and `name` is filesystem-unsafe, a field exceeds the
/// size cap, the skill is unknown, the atomic write fails, or the version-snapshot insert fails.
pub async fn edit_skill(kms: &KmsHandle, name: &str, kind: SkillEditKind, draft: &SkillDraft) -> Result<String> {
    if kind == SkillEditKind::Authored {
        validate_skill_name(name)?;
    }
    validate_draft(draft)?;
    snapshot_current(kms, name, kind, None).await?;

    let new_body = render_markdown(draft);
    match kind {
        SkillEditKind::Authored => write_authored_body(kms, name, &new_body).await,
        SkillEditKind::Synthesized => write_synthesized_body(kms, name, &new_body).await,
    }
}

/// Revert `name` to the exact content recorded in `version_id` — dispatches by the version
/// row's OWN `kind` ("re-applies the atomic edit path for the kind"), not a caller-supplied one,
/// so a stale/mismatched argument can never revert into the wrong store.
///
/// # Errors
/// Returns an error if the version id is unknown, belongs to a different skill, or the
/// write-back fails.
pub async fn revert_skill(kms: &KmsHandle, name: &str, version_id: &str) -> Result<String> {
    let version = skill_versions::get_version(&kms.db, version_id)
        .await?
        .ok_or_else(|| anyhow!("unknown skill version '{version_id}'"))?;
    if version.skill_name != name {
        bail!("version '{version_id}' does not belong to skill '{name}'");
    }
    let kind = SkillEditKind::parse(&version.kind)?;
    snapshot_current(kms, name, kind, Some("pre-revert snapshot")).await?;
    match kind {
        SkillEditKind::Authored => write_authored_full(kms, name, &version.content_md).await,
        SkillEditKind::Synthesized => write_synthesized_body(kms, name, &version.content_md).await,
    }
}

/// Promote a synthesized skill to an authored kit-pack file (D4) — exits the confidence/decay
/// lifecycle permanently. Renders its current description as the body (via `parse_markdown`'s
/// free-form fallback when opened later) and synthesizes minimal frontmatter: domain-agnostic
/// (a synthesized row has no domain field), `when_to_use` mirrors the trigger `pattern` (the
/// closest existing analogue), `kind` is `Playbook` (reusable how-to, not a pipeline stage).
/// Archives the synthesized row in the same call so no duplicate stays active.
///
/// # Ordering (review LOW — shrinking the cross-store duplicate window)
/// Steps run file-write → version log → archive → REGISTRY RELOAD, deliberately deferring the
/// reload (unlike `edit`/`revert`, which reload immediately via `write_authored_full`) until
/// AFTER the synthesized row is archived. This means the in-memory `AuthoredRegistry` never
/// advertises the promoted skill while the synthesized row is still active — the only window
/// that remains is a crash between "authored file committed" and "row archived", where:
///
///   - the on-disk kit-pack has the new file (harmless: nothing has reloaded it into the
///     registry yet, so no consumer sees it);
///   - the synthesized row is still active, so `synthesized_playbooks_for`/`active_skills` keep
///     injecting it exactly as before.
///
/// Neither side "wins" a race, because there is no race: only ONE store is ever live-reachable
/// at a time from this function's own reload boundary. A retried `promote_to_authored` after
/// such a crash simply overwrites the same (already-correct) file and completes the archive —
/// idempotent, not a duplicate. The residual DB-only risk (crash between version-insert and
/// archive, leaving an orphan version row with no matching archive) is pure bookkeeping — no
/// injection-visible effect — and is accepted (see the phase's Risk Assessment).
///
/// # Errors
/// Returns an error if `name` is invalid, the synthesized skill is unknown, an authored skill
/// of the same name already exists (promote never overwrites), or either write fails.
pub async fn promote_to_authored(kms: &KmsHandle, name: &str) -> Result<String> {
    validate_skill_name(name)?;
    if kms.authored_get(name).is_some() {
        bail!("an authored skill named '{name}' already exists — promote refuses to overwrite it");
    }
    let row = db_skills::get_skill_by_name(&kms.db, name)
        .await?
        .ok_or_else(|| anyhow!("unknown synthesized skill '{name}'"))?;

    let new_skill = AuthoredSkill {
        name: name.to_string(),
        description: row.description.clone(),
        when_to_use: row.pattern.clone(),
        domain: None,
        specialists: Vec::new(),
        kind: SkillKind::Playbook,
        body: row.description.clone(),
        references: Vec::new(),
        recovered: false,
    };
    let full_md = write_authored_file(kms, name, &new_skill.to_markdown()).await?;

    let hash = sha256_hex(full_md.as_bytes());
    skill_versions::insert_version(
        &kms.db,
        name,
        SkillEditKind::Authored.as_str(),
        &full_md,
        &hash,
        Some("promoted from synthesized"),
    )
    .await?;

    db_skills::archive_skill(&kms.db, &row.id).await?;
    kms.reload_authored().await?;
    Ok(full_md)
}

/// Manually archive a synthesized skill (D4) — a thin, explicitly-named wrapper over the
/// existing decay-lifecycle archival query, so the editor's Archive action does not need to
/// import `haily_db::queries::skills` directly.
///
/// # Errors
/// Returns an error if the skill is unknown or the archive write fails.
pub async fn archive_synthesized(kms: &KmsHandle, name: &str) -> Result<()> {
    let row = db_skills::get_skill_by_name(&kms.db, name)
        .await?
        .ok_or_else(|| anyhow!("unknown synthesized skill '{name}'"))?;
    db_skills::archive_skill(&kms.db, &row.id).await
}

/// Replace `name`'s authored BODY only — frontmatter is carried through unchanged from the
/// currently-loaded record.
async fn write_authored_body(kms: &KmsHandle, name: &str, new_body: &str) -> Result<String> {
    let mut skill = kms.authored_get(name).ok_or_else(|| anyhow!("unknown authored skill '{name}'"))?;
    skill.body = new_body.to_string();
    write_authored_full(kms, name, &skill.to_markdown()).await
}

/// Write `full_md` (frontmatter + body) as `name`'s authored file: atomic skill-file + manifest
/// write (manifest rename is the commit point — see `kit_pack::write_skill_atomic`). Does NOT
/// reload the registry — `promote_to_authored` deliberately defers that (see its doc comment);
/// every other caller goes through `write_authored_full` below, which reloads immediately.
///
/// The whole read-manifest→write-file→write-manifest sequence runs under
/// `authored_write_lock()` (review MED-2) so two concurrent authored writes (to different
/// skills) can't race the shared `manifest.json`.
async fn write_authored_file(kms: &KmsHandle, name: &str, full_md: &str) -> Result<String> {
    validate_skill_name(name)?;
    let dir = kms.kit_pack_dir().ok_or_else(|| anyhow!("kit-pack not found — cannot write authored skill"))?;
    let kind = kms.authored_get(name).map(|s| s.kind).unwrap_or(SkillKind::Playbook);

    let _guard = authored_write_lock().lock().await;
    let rel_path = kit_pack::skill_rel_path(&dir, name, kind);
    kit_pack::write_skill_atomic(&dir, &rel_path, full_md)?;
    Ok(full_md.to_string())
}

/// Write + immediately reload the registry — the edit/revert path, where the caller needs the
/// change visible right away (unlike `promote_to_authored`, see `write_authored_file`'s doc).
async fn write_authored_full(kms: &KmsHandle, name: &str, full_md: &str) -> Result<String> {
    let saved = write_authored_file(kms, name, full_md).await?;
    kms.reload_authored().await?;
    Ok(saved)
}

/// Replace a synthesized skill's `description` column with the rendered body.
async fn write_synthesized_body(kms: &KmsHandle, name: &str, new_body: &str) -> Result<String> {
    let row = db_skills::get_skill_by_name(&kms.db, name)
        .await?
        .ok_or_else(|| anyhow!("unknown synthesized skill '{name}'"))?;
    db_skills::update_skill_body(&kms.db, &row.id, new_body).await?;
    Ok(new_body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::DbHandle;
    use std::collections::BTreeMap;

    /// Build a `<tmp>/kit-pack` dir with one skill (+ a reference chunk it must never lose) and
    /// a `<tmp>/data` dir, then a fully-initialized `KmsHandle` over both. Mirrors
    /// `kit_pack::tests::make_pack` but rooted under a `data_dir` the packaged-location
    /// precedence check (`KmsHandle::kit_pack_source`) will actually find.
    async fn test_kms() -> (KmsHandle, DbHandle, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        let pack_dir = data_dir.join("kit-pack");
        std::fs::create_dir_all(pack_dir.join("skills")).unwrap();
        std::fs::create_dir_all(pack_dir.join("references/cook")).unwrap();

        let skill_body = "---\nname: cook\ndescription: build code\nwhen_to_use: implement a change\ndomain: developer\nkind: stage-prompt\n---\n## Procedure\noriginal steps\n";
        let ref_body = "# TDD\ntests first\n";
        std::fs::write(pack_dir.join("skills/cook.md"), skill_body).unwrap();
        std::fs::write(pack_dir.join("references/cook/tdd.md"), ref_body).unwrap();

        let mut files = BTreeMap::new();
        files.insert("skills/cook.md".to_string(), sha256_hex(skill_body.as_bytes()));
        files.insert("references/cook/tdd.md".to_string(), sha256_hex(ref_body.as_bytes()));
        let manifest = serde_json::json!({ "version": "1", "source_commit": "test", "files": files });
        std::fs::write(pack_dir.join("manifest.json"), serde_json::to_string(&manifest).unwrap()).unwrap();

        let db = DbHandle::init(&data_dir.join("t.db")).await.unwrap();
        let kms = KmsHandle::init(db.clone(), &data_dir).await.unwrap();
        (kms, db, root)
    }

    fn draft(procedure: &str) -> SkillDraft {
        SkillDraft { procedure: procedure.to_string(), ..Default::default() }
    }

    #[tokio::test]
    async fn edit_authored_updates_file_manifest_registry_and_logs_a_version() {
        let (kms, db, root) = test_kms().await;
        let pack_dir = root.path().join("data/kit-pack");

        edit_skill(&kms, "cook", SkillEditKind::Authored, &draft("NEW STEPS")).await.unwrap();

        let on_disk = std::fs::read_to_string(pack_dir.join("skills/cook.md")).unwrap();
        assert!(on_disk.contains("NEW STEPS"));

        assert!(kms.authored_get("cook").unwrap().body.contains("NEW STEPS"), "registry must reflect the edit");

        let versions = skill_versions::list_versions(&db, "cook").await.unwrap();
        assert_eq!(versions.len(), 1, "the pre-edit content must be snapshotted");
        assert!(versions[0].content_md.contains("original steps"));
    }

    #[tokio::test]
    async fn edit_rejects_a_traversal_name() {
        let (kms, _db, _root) = test_kms().await;
        let result = edit_skill(&kms, "../escape", SkillEditKind::Authored, &draft("x")).await;
        assert!(result.is_err(), "a traversal name must never reach the write path");
    }

    #[tokio::test]
    async fn edit_rejects_an_oversized_field() {
        let (kms, _db, _root) = test_kms().await;
        let huge = "x".repeat(MAX_FIELD_BYTES + 1);
        let result = edit_skill(&kms, "cook", SkillEditKind::Authored, &draft(&huge)).await;
        assert!(result.is_err(), "an oversized field must be rejected before any write");
    }

    #[tokio::test]
    async fn revert_restores_the_original_content() {
        let (kms, db, _root) = test_kms().await;
        edit_skill(&kms, "cook", SkillEditKind::Authored, &draft("FIRST EDIT")).await.unwrap();
        edit_skill(&kms, "cook", SkillEditKind::Authored, &draft("SECOND EDIT")).await.unwrap();

        let versions = skill_versions::list_versions(&db, "cook").await.unwrap();
        // Oldest recorded version is the ORIGINAL (pre-first-edit) content.
        let original_version = versions.last().unwrap();
        assert!(original_version.content_md.contains("original steps"));

        revert_skill(&kms, "cook", &original_version.id).await.unwrap();
        assert!(
            kms.authored_get("cook").unwrap().body.contains("original steps"),
            "revert must restore the pre-edit content"
        );
    }

    #[tokio::test]
    async fn edit_synthesized_updates_description_and_logs_a_version() {
        let (kms, db, _root) = test_kms().await;
        db_skills::insert_skill(&db, "my-synth", "old description", "trigger pattern", "[]").await.unwrap();

        edit_skill(&kms, "my-synth", SkillEditKind::Synthesized, &draft("NEW SYNTH BODY")).await.unwrap();

        let row = db_skills::get_skill_by_name(&db, "my-synth").await.unwrap().unwrap();
        assert!(row.description.contains("NEW SYNTH BODY"));

        let versions = skill_versions::list_versions(&db, "my-synth").await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].kind, "synthesized");
    }

    #[tokio::test]
    async fn promote_creates_an_authored_file_and_archives_the_synthesized_row() {
        let (kms, db, root) = test_kms().await;
        db_skills::insert_skill(&db, "grow-up", "a useful synthesized playbook", "when growing up", "[]")
            .await
            .unwrap();

        promote_to_authored(&kms, "grow-up").await.unwrap();

        assert!(kms.authored_get("grow-up").is_some(), "promoted skill must be authored now");
        assert!(
            std::fs::read_to_string(root.path().join("data/kit-pack/skills/grow-up.md"))
                .unwrap()
                .contains("a useful synthesized playbook")
        );
        assert!(
            db_skills::get_skill_by_name(&db, "grow-up").await.unwrap().is_none(),
            "the synthesized row must be archived (no longer active), not left duplicated"
        );
    }

    #[tokio::test]
    async fn promote_refuses_to_overwrite_an_existing_authored_skill() {
        let (kms, db, _root) = test_kms().await;
        db_skills::insert_skill(&db, "cook", "a synthesized duplicate of the authored cook", "x", "[]")
            .await
            .unwrap();
        assert!(promote_to_authored(&kms, "cook").await.is_err());
    }

    #[tokio::test]
    async fn archive_synthesized_marks_the_row_archived() {
        let (kms, db, _root) = test_kms().await;
        db_skills::insert_skill(&db, "to-archive", "d", "p", "[]").await.unwrap();
        archive_synthesized(&kms, "to-archive").await.unwrap();
        assert!(db_skills::get_skill_by_name(&db, "to-archive").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn synthesized_edit_accepts_a_name_that_would_fail_the_authored_filename_guard() {
        // Review LOW: the filename-safety allowlist must gate ONLY the authored (filesystem)
        // path — a synthesized row is never a path component.
        let (kms, db, _root) = test_kms().await;
        let exotic_name = "a synthesized skill (v2)!";
        db_skills::insert_skill(&db, exotic_name, "old description", "p", "[]").await.unwrap();

        // Precondition: this name would be rejected by the authored guard.
        assert!(validate_skill_name(exotic_name).is_err());

        edit_skill(&kms, exotic_name, SkillEditKind::Synthesized, &draft("SAVED")).await.unwrap();
        let row = db_skills::get_skill_by_name(&db, exotic_name).await.unwrap().unwrap();
        assert!(row.description.contains("SAVED"));
    }

    #[tokio::test]
    async fn concurrent_edits_to_different_skills_do_not_clobber_the_shared_manifest() {
        // Review MED-2 regression guard: two authored writes racing the SAME manifest.json
        // must both land, not have the second's stale read-copy clobber the first's hash.
        let (kms, db, root) = test_kms().await;
        db_skills::insert_skill(&db, "second", "a second skill", "p", "[]").await.unwrap();
        promote_to_authored(&kms, "second").await.unwrap();

        let cook_draft = draft("COOK EDITED");
        let second_draft = draft("SECOND EDITED");
        let (r1, r2) = tokio::join!(
            edit_skill(&kms, "cook", SkillEditKind::Authored, &cook_draft),
            edit_skill(&kms, "second", SkillEditKind::Authored, &second_draft),
        );
        r1.unwrap();
        r2.unwrap();

        // Reload straight from disk (bypassing the live registry) to prove the manifest ITSELF
        // is consistent for both entries, not just whichever reload ran last.
        let pack_dir = root.path().join("data/kit-pack");
        let reloaded = kit_pack::load_with_versions_fallback(&pack_dir, &db).await.unwrap();
        let cook = reloaded.iter().find(|s| s.name == "cook").unwrap();
        let second = reloaded.iter().find(|s| s.name == "second").unwrap();
        assert!(cook.body.contains("COOK EDITED"), "cook's write must survive second's concurrent write");
        assert!(second.body.contains("SECOND EDITED"), "second's write must survive cook's concurrent write");
        assert!(!cook.recovered && !second.recovered, "neither hash should mismatch under the write lock");
    }

    #[tokio::test]
    async fn get_skill_detail_maps_authored_and_synthesized_bodies() {
        let (kms, db, _root) = test_kms().await;
        db_skills::insert_skill(&db, "synth-one", "plain synthesized description", "p", "[]").await.unwrap();

        let authored = get_skill_detail(&kms, "cook", SkillEditKind::Authored).await.unwrap();
        assert!(authored.draft.procedure.contains("original steps"));

        let synthesized = get_skill_detail(&kms, "synth-one", SkillEditKind::Synthesized).await.unwrap();
        assert_eq!(synthesized.draft.procedure, "plain synthesized description");
    }
}
