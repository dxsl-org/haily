//! Runtime-mediated skill discovery + lazy-load tools (progressive disclosure).
//!
//! These are the mechanical equivalent of Claude Code's Read/Skill mediation: the model
//! never reads a skill file itself — it emits one of these tool calls, and the host
//! runtime (`haily-tools`) reads the chunk from the KMS authored registry and returns
//! it. Available at L0 and to every sub-agent — the universal, model-initiated lazy-load
//! path that keeps the L0 prompt bounded (the index is inlined; bodies/references are
//! pulled on demand).
//!
//! All three are [`RiskTier::Read`]: they only read in-memory authored content, never
//! touch the DB, network, or filesystem.

use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Cap on how many discovery hits `skill_search` returns — keeps the model's context
/// bounded even as the kit-pack grows.
const SEARCH_LIMIT: usize = 8;

/// `skill_search(query)` — discovery half of the lazy-load: find authored skills
/// relevant to `query`, returning each skill's name + when_to_use.
pub struct SkillSearchTool;

#[async_trait]
impl Tool for SkillSearchTool {
    fn name(&self) -> &str {
        "skill_search"
    }
    fn description(&self) -> &str {
        "Tìm skill/playbook liên quan tới một truy vấn. Trả về tên skill + khi nào nên dùng. Dùng trước skill_fetch."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Mô tả việc cần làm để tìm skill phù hợp." }
            },
            "required": ["query"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let query = args["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Ok("Cần cung cấp 'query' để tìm skill.".to_string());
        }
        let hits = ctx.kms.search_skills(query, SEARCH_LIMIT);
        if hits.is_empty() {
            return Ok("Không tìm thấy skill phù hợp.".to_string());
        }
        Ok(hits
            .into_iter()
            .map(|(name, when)| format!("- {name}: {when}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// `skill_list_sections(skill)` — enumerate the fetchable chunks of a skill (its `body`
/// plus each reference chunk), each with a one-line summary, so the model can choose
/// exactly one to pull.
pub struct SkillListSectionsTool;

#[async_trait]
impl Tool for SkillListSectionsTool {
    fn name(&self) -> &str {
        "skill_list_sections"
    }
    fn description(&self) -> &str {
        "Liệt kê các section (body + references) của một skill để biết cái nào có thể fetch. Dùng trước skill_fetch."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Tên skill (từ skill_search)." }
            },
            "required": ["skill"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let skill = args["skill"].as_str().unwrap_or("").trim();
        if skill.is_empty() {
            return Ok("Cần cung cấp 'skill'.".to_string());
        }
        // An unknown skill surfaces as a tool error (not an empty list) — dispatch renders
        // it back to the model, which then knows to skill_search again.
        let sections = ctx.kms.list_skill_sections(skill)?;
        Ok(sections
            .into_iter()
            .map(|(id, summary)| format!("- {id}: {summary}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// `skill_fetch(skill, section)` — pull exactly ONE chunk (progressive-disclosure level
/// 2/3). `section` defaults to `body`. An unknown section returns an error, never a dump
/// of the whole skill.
pub struct SkillFetchTool;

#[async_trait]
impl Tool for SkillFetchTool {
    fn name(&self) -> &str {
        "skill_fetch"
    }
    fn description(&self) -> &str {
        "Lấy nội dung MỘT section của skill (mặc định 'body', hoặc một reference chunk). Chỉ một chunk mỗi lần."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Tên skill." },
                "section": { "type": "string", "description": "Section id (mặc định 'body'; hoặc id từ skill_list_sections)." }
            },
            "required": ["skill"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let skill = args["skill"].as_str().unwrap_or("").trim();
        if skill.is_empty() {
            return Ok("Cần cung cấp 'skill'.".to_string());
        }
        // Default to the skill's body when no section is named.
        let section = args["section"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("body");
        // Unknown skill/section → error (dispatch surfaces it), never a full-skill dump.
        let chunk = ctx.kms.fetch_skill_section(skill, section)?;
        Ok(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;
    use haily_db::DbHandle;
    use haily_kms::KmsHandle;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    /// Throwaway approval gate — these Read-tier tools never raise an approval, but
    /// `ToolContext` requires a gate handle.
    struct NoopGate;
    #[async_trait]
    impl haily_types::ApprovalGate for NoopGate {
        async fn request(
            &self,
            _approval_id: Uuid,
            _session_id: Uuid,
            _cancel: &CancellationToken,
        ) -> bool {
            false
        }
    }

    /// A `ToolContext` whose KMS loads the CWD-relative `assets/kit-pack` — the test
    /// runs from the repo root under `cargo test`, so the shipped pack is present. When
    /// it is not (isolated run), the assertions that require content are skipped.
    async fn ctx_with_kms(dir: &std::path::Path) -> (ToolContext, Arc<KmsHandle>) {
        let db = Arc::new(DbHandle::init(&dir.join("t.db")).await.unwrap());
        let kms = Arc::new(KmsHandle::init((*db).clone(), dir).await.unwrap());
        let (tx, _rx) = mpsc::channel(8);
        let ctx = ToolContext {
            db,
            kms: Arc::clone(&kms),
            session_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            depth: 0,
            domain: None,
            approval_gate: Arc::new(NoopGate),
            approval_tx: tx,
            cancel: CancellationToken::new(),
            turn_deletes: Arc::new(AtomicUsize::new(0)),
            last_journal_id: Arc::new(Mutex::new(None)),
        };
        (ctx, kms)
    }

    #[tokio::test]
    async fn skill_fetch_unknown_section_errors_not_dumps() {
        let dir = tempfile::tempdir().unwrap();
        // Copy the shipped kit-pack into data_dir so init loads it deterministically.
        if !copy_repo_kit_pack(dir.path()) {
            return; // shipped pack unavailable in this run — nothing to assert
        }
        let (ctx, _kms) = ctx_with_kms(dir.path()).await;

        let tool = SkillFetchTool;
        // Known skill 'cook', unknown section → must be an Err (surfaced as a tool error),
        // never a body dump.
        let res = tool
            .execute(serde_json::json!({"skill":"cook","section":"nope"}), &ctx)
            .await;
        assert!(res.is_err(), "unknown section must error, not dump");

        // body pull returns the cook body (level 2), NOT the reference chunk (level 3).
        let body = tool
            .execute(serde_json::json!({"skill":"cook"}), &ctx)
            .await
            .expect("body fetch");
        assert!(body.contains("Cook Stage"), "body should contain the cook stage prompt");
        assert!(
            !body.to_lowercase().contains("tdd workflow (reference)"),
            "the reference chunk must NOT be part of the body"
        );
    }

    #[tokio::test]
    async fn skill_search_finds_a_coding_skill() {
        let dir = tempfile::tempdir().unwrap();
        if !copy_repo_kit_pack(dir.path()) {
            return;
        }
        let (ctx, _kms) = ctx_with_kms(dir.path()).await;
        let out = SkillSearchTool
            .execute(serde_json::json!({"query":"fix a compile bug"}), &ctx)
            .await
            .expect("search");
        assert!(out.contains("fix"), "expected the fix skill in results: {out}");
    }

    /// Copy `assets/kit-pack` (relative to the repo root) into `<data>/kit-pack`. Returns
    /// false if the shipped pack cannot be found. The crate dir is `crates/haily-tools`,
    /// so the repo root is two levels up.
    fn copy_repo_kit_pack(data_dir: &std::path::Path) -> bool {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/kit-pack");
        if !src.join("manifest.json").is_file() {
            return false;
        }
        let dst = data_dir.join("kit-pack");
        copy_dir(&src, &dst);
        true
    }

    fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let target = dst.join(entry.file_name());
            if path.is_dir() {
                copy_dir(&path, &target);
            } else {
                std::fs::copy(&path, &target).unwrap();
            }
        }
    }
}
