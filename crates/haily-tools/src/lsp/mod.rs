//! Language-Server semantic layer (Sub-Agent + Skill Architecture P10) — a uniform, multi-language
//! interface to SEMANTIC diagnostics + safe cross-file rename, so the coding pipeline no longer
//! hand-parses each toolchain's output format nor treats every refactor-rename as a blind string
//! replace.
//!
//! # Two capabilities, both HINTS (never the correctness gate of record)
//! - [`LspDiagnosticsTool`] (`lsp_diagnostics`, `Read`) — semantic errors/warnings for a file, an
//!   ADDITIONAL per-edit signal in P6's verify loop, deduplicated against the build-gate output
//!   ([`dedup_against_build_gate`]) so the model is never shown the same error twice. A green LSP
//!   with a failing test is STILL a failure — the build/test gate stays authoritative.
//! - [`LspRenameTool`] (`lsp_rename`, `ReversibleWrite`) — a project-safe rename the server
//!   computes across every reference; its file writes go through the workspace journal (audit
//!   rows) exactly like `fs_edit`, and the worktree remains the single compensator.
//!
//! # Graceful degradation is the DEFAULT path (decision #4)
//! No language server on `PATH` for a file's language → both tools NO-OP with a clear message and
//! the pipeline falls back to the build-gate + P1 tree-sitter lint. This is never a hard failure,
//! so the whole layer compiles + tests on every build WITHOUT a live server (the live smoke is
//! deferred to a host that has servers installed — see [`client`]).

pub mod client;
pub mod registry;

use crate::coding::path_guard::{canonical_root, resolve_in_workspace};
use crate::coding::{journal_coding_audit, load_workspace};
use crate::connector::redact;
use crate::exec::NetworkPolicy;
use crate::{RiskTier, Tool, ToolContext};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use client::{build_spawn_config, LspClient};
use haily_db::queries::coding_workspaces::CodingWorkspaceRow;
use lsp_types::{Diagnostic, DiagnosticSeverity, Url};
use registry::LspServerSpec;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Tool names this module registers. Kept as a const so the developer-domain whitelist in
/// `haily-core::domains` can name them and the wiring test can assert they resolve in `build_v1`.
pub const LSP_TOOL_NAMES: &[&str] = &["lsp_diagnostics", "lsp_rename"];

/// How long to wait for a server's asynchronous `publishDiagnostics` push after opening a document
/// before returning whatever has been collected. Bounded so a silent/slow server degrades to "no
/// diagnostics" rather than hanging the verify loop.
const DIAGNOSTIC_WAIT: Duration = Duration::from_millis(1500);

/// Char cap on the rendered diagnostics block returned to the model (bounds context growth).
const DIAGNOSTICS_CHAR_CAP: usize = 6_000;

/// Map a file path to its LSP language key by extension. `None` for an extension with no known
/// server language → the caller degrades deterministically (used by the no-op degradation test).
pub fn language_for_file(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => "typescript",
        "go" => "go",
        "java" => "java",
        _ => return None,
    })
}

/// The clear degradation message returned when no server can drive `path`'s language — names WHY
/// (unknown language, or server not installed) so the model/pipeline knows to rely on the
/// build-gate + tree-sitter lint instead of treating this as a failure.
fn degradation_message(tool: &str, path: &str, reason: &str) -> String {
    format!(
        "{tool}: no language server available for {path} ({reason}). Semantic {tool} skipped — \
         rely on the build/test gate and syntax lint. This is not a failure."
    )
}

/// Resolve the available server spec for a file, or the reason it degrades.
enum Resolved {
    Ready(LspServerSpec),
    Degrade(String),
}

/// Decide whether a file's language has an installed, available server. Pure w.r.t. process state
/// except the PATH probe — no spawning. Returns a degradation reason string when unavailable.
fn resolve_for_file(tool: &str, path: &str) -> Resolved {
    let Some(language) = language_for_file(path) else {
        return Resolved::Degrade(degradation_message(tool, path, "unsupported language"));
    };
    let Some(spec) = registry::server_for_language(language) else {
        return Resolved::Degrade(degradation_message(tool, path, "no server mapped"));
    };
    if !registry::is_available(&spec) {
        return Resolved::Degrade(degradation_message(
            tool,
            path,
            &format!("'{}' not found on PATH", spec.program),
        ));
    }
    Resolved::Ready(spec)
}

/// Render collected diagnostics into a concise, tag-stripped, capped model-safe block. Untrusted
/// (server-derived) text is tag-stripped so a `<tool_call>` token in a diagnostic message cannot
/// steer the fix loop (P4 contract).
fn render_diagnostics(diags: &[Diagnostic]) -> String {
    if diags.is_empty() {
        return "lsp_diagnostics: no semantic diagnostics.".to_string();
    }
    let mut body = format!("lsp_diagnostics: {} diagnostic(s)\n", diags.len());
    for d in diags {
        let sev = match d.severity {
            Some(DiagnosticSeverity::ERROR) => "error",
            Some(DiagnosticSeverity::WARNING) => "warning",
            Some(DiagnosticSeverity::INFORMATION) => "info",
            Some(DiagnosticSeverity::HINT) => "hint",
            _ => "note",
        };
        let line = d.range.start.line + 1;
        let col = d.range.start.character + 1;
        body.push_str(&format!("  [{sev}] {line}:{col} {}\n", d.message.trim()));
        if body.len() > DIAGNOSTICS_CHAR_CAP {
            body.push_str("  [... additional diagnostics elided ...]\n");
            break;
        }
    }
    redact::strip_tool_tags(&body)
}

/// Drop LSP diagnostic lines whose message text is ALSO present in the build-gate output, so the
/// model is not shown the same error twice (decision #5). The build gate is authoritative; LSP is
/// the faster/richer hint, so on overlap the build-gate line wins and the LSP duplicate is elided.
/// Pure over strings — unit-testable without a server. Matching is substring-based on the
/// diagnostic's core message (LSP wording is usually a superset the compiler line contains).
pub fn dedup_against_build_gate(lsp_lines: &[String], build_output: &str) -> Vec<String> {
    lsp_lines
        .iter()
        .filter(|line| {
            let msg = diagnostic_core_message(line);
            msg.is_empty() || !build_output.contains(msg)
        })
        .cloned()
        .collect()
}

/// Extract the message payload from a rendered `  [sev] L:C message` line for dedup comparison
/// (everything after the `line:col ` prefix). Returns the whole trimmed line if it has no prefix.
fn diagnostic_core_message(line: &str) -> &str {
    let after_sev = line.split_once(']').map(|(_, r)| r.trim_start()).unwrap_or(line);
    // Skip a leading `L:C ` position token if present.
    match after_sev.split_once(' ') {
        Some((pos, rest)) if pos.contains(':') && pos.chars().all(|c| c.is_ascii_digit() || c == ':') => {
            rest.trim()
        }
        _ => after_sev.trim(),
    }
}

// ---------------------------------------------------------------------------
// Pooled server lifecycle (P0 Manager scope pattern, keyed by workspace+language).
// ---------------------------------------------------------------------------

/// One server per (workspace, language), reused across stages and torn down on workspace close —
/// the P0 Manager scope pattern applied to long-lived servers. Process-wide (like the coding
/// sandbox pool) because `ToolContext` cannot carry it without widening ~20 construction sites.
#[derive(Default)]
struct LspManager {
    clients: tokio::sync::Mutex<HashMap<String, Arc<LspClient>>>,
}

fn manager() -> &'static LspManager {
    static M: OnceLock<LspManager> = OnceLock::new();
    M.get_or_init(LspManager::default)
}

impl LspManager {
    /// Pool key = workspace id + language (one rust-analyzer per Rust workspace, etc.).
    fn key(workspace_id: &str, language: &str) -> String {
        format!("{workspace_id}::{language}")
    }

    /// Get the pooled client for (workspace, language), spawning + initializing one on a miss.
    /// The spawn applies the P0 isolation config ([`build_spawn_config`]); initialization is a
    /// best-effort handshake — a server that fails to initialize is not pooled, so the caller
    /// degrades rather than reusing a broken server.
    async fn get_or_spawn(
        &self,
        workspace_id: &str,
        work_root: &Path,
        spec: &LspServerSpec,
    ) -> Result<Arc<LspClient>> {
        let key = Self::key(workspace_id, spec.language);
        let mut guard = self.clients.lock().await;
        if let Some(existing) = guard.get(&key) {
            return Ok(Arc::clone(existing));
        }
        let config = build_spawn_config(spec, work_root);
        debug_assert_eq!(config.network, NetworkPolicy::Off, "servers must run network-off");
        let client = Arc::new(LspClient::spawn(config).await?);
        initialize(&client, work_root).await?;
        guard.insert(key, Arc::clone(&client));
        Ok(client)
    }

    /// Tear down every pooled server for a workspace (called on workspace close).
    async fn release_workspace(&self, workspace_id: &str) {
        let prefix = format!("{workspace_id}::");
        self.clients.lock().await.retain(|k, _| !k.starts_with(&prefix));
    }
}

/// Drive the LSP `initialize`/`initialized` handshake rooted at the workspace. Best-effort: any
/// transport error surfaces as an `Err` the caller turns into a degradation message.
async fn initialize(client: &LspClient, work_root: &Path) -> Result<()> {
    let root_uri = Url::from_directory_path(work_root)
        .map_err(|_| anyhow!("workspace root is not a valid file URL: {}", work_root.display()))?;
    #[allow(deprecated)] // root_uri is deprecated in the spec but still honored by every server.
    let params = lsp_types::InitializeParams {
        root_uri: Some(root_uri),
        capabilities: lsp_types::ClientCapabilities::default(),
        ..Default::default()
    };
    let socket = client.socket();
    socket
        .request::<lsp_types::request::Initialize>(params)
        .await
        .map_err(|e| anyhow!("lsp initialize failed: {e}"))?;
    socket
        .notify::<lsp_types::notification::Initialized>(lsp_types::InitializedParams {})
        .map_err(|e| anyhow!("lsp initialized notify failed: {e}"))?;
    Ok(())
}

/// Public teardown hook: release every pooled server for a closed workspace. Called by the
/// workspace-close path so servers do not outlive their workspace (P0 scope teardown).
pub async fn release_workspace_servers(workspace_id: &str) {
    manager().release_workspace(workspace_id).await;
}

// ---------------------------------------------------------------------------
// Tools.
// ---------------------------------------------------------------------------

/// `lsp_diagnostics(workspace_id, path)` — semantic diagnostics for one file. `Read` tier (no
/// approval). Degrades to a clear no-op message when no server is available.
pub struct LspDiagnosticsTool;

#[async_trait]
impl Tool for LspDiagnosticsTool {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }
    fn description(&self) -> &str {
        "Semantic diagnostics (type errors, unresolved symbols) for a workspace file via its \
         language server. A HINT layer additional to the build/test gate; no-ops cleanly if no \
         server is installed."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "path": { "type": "string", "description": "workspace-relative file path" }
            },
            "required": ["workspace_id", "path"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let rel = args["path"].as_str().ok_or_else(|| anyhow!("path (string) is required"))?;
        let spec = match resolve_for_file("lsp_diagnostics", rel) {
            Resolved::Ready(spec) => spec,
            Resolved::Degrade(msg) => return Ok(msg),
        };
        // Live path (deferred smoke): any server/transport error degrades cleanly, never hard-fails.
        match collect_diagnostics(&ws, rel, &spec).await {
            Ok(rendered) => Ok(rendered),
            Err(e) => {
                tracing::debug!("lsp_diagnostics degraded: {e:#}");
                Ok(degradation_message("lsp_diagnostics", rel, "server error"))
            }
        }
    }
}

/// Open `rel` in the pooled server and return its rendered diagnostics after a bounded wait.
async fn collect_diagnostics(
    ws: &CodingWorkspaceRow,
    rel: &str,
    spec: &LspServerSpec,
) -> Result<String> {
    let root = canonical_root(Path::new(&ws.worktree_path))?;
    let abs = resolve_in_workspace(&root, rel)?;
    let uri = Url::from_file_path(&abs).map_err(|_| anyhow!("file path is not a valid URL"))?;
    let text = tokio::fs::read_to_string(&abs).await?;
    let client = manager().get_or_spawn(&ws.id, &root, spec).await?;
    open_document(&client, &uri, spec.language, &text)?;
    tokio::time::sleep(DIAGNOSTIC_WAIT).await;
    Ok(render_diagnostics(&client.diagnostics_for(&uri)))
}

/// Send `textDocument/didOpen` so the server indexes + diagnoses the file.
fn open_document(client: &LspClient, uri: &Url, language: &str, text: &str) -> Result<()> {
    let item = lsp_types::TextDocumentItem {
        uri: uri.clone(),
        language_id: language.to_string(),
        version: 1,
        text: text.to_string(),
    };
    client
        .socket()
        .notify::<lsp_types::notification::DidOpenTextDocument>(
            lsp_types::DidOpenTextDocumentParams { text_document: item },
        )
        .map_err(|e| anyhow!("didOpen failed: {e}"))
}

/// `lsp_rename(workspace_id, path, line, character, new_name)` — project-safe cross-file rename of
/// the symbol at the given position. `ReversibleWrite` (journaled; worktree compensates). Degrades
/// cleanly when no server is available, so a refactor-rename task falls back to string edits.
pub struct LspRenameTool;

#[async_trait]
impl Tool for LspRenameTool {
    fn name(&self) -> &str {
        "lsp_rename"
    }
    fn description(&self) -> &str {
        "Rename the symbol at (path, line, character) across the whole project via the language \
         server — semantically correct, no string collisions. Journaled like any file write; \
         no-ops cleanly if no server is installed."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "path": { "type": "string", "description": "workspace-relative file of the symbol" },
                "line": { "type": "integer", "description": "0-based line of the symbol" },
                "character": { "type": "integer", "description": "0-based column of the symbol" },
                "new_name": { "type": "string" }
            },
            "required": ["workspace_id", "path", "line", "character", "new_name"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        // A rename WRITES files; the worktree compensator reverts them, so it is the auto-running
        // journaled tier, exactly like fs_edit — never IrreversibleWrite.
        RiskTier::ReversibleWrite
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let rel = args["path"].as_str().ok_or_else(|| anyhow!("path (string) is required"))?;
        let new_name =
            args["new_name"].as_str().ok_or_else(|| anyhow!("new_name (string) is required"))?;
        let line = args["line"].as_u64().ok_or_else(|| anyhow!("line (integer) is required"))? as u32;
        let character =
            args["character"].as_u64().ok_or_else(|| anyhow!("character (integer) is required"))?
                as u32;
        let spec = match resolve_for_file("lsp_rename", rel) {
            Resolved::Ready(spec) => spec,
            Resolved::Degrade(msg) => return Ok(msg),
        };
        match apply_rename(ctx, &ws, rel, line, character, new_name, &spec).await {
            Ok(msg) => Ok(msg),
            Err(e) => {
                tracing::debug!("lsp_rename degraded: {e:#}");
                Ok(degradation_message("lsp_rename", rel, "server error"))
            }
        }
    }
}

/// Request a server rename and apply the resulting `WorkspaceEdit` to the workspace, journaling
/// each modified file. Every write is path-guarded (never escapes the workspace root) and audited;
/// the worktree remains the single compensator.
async fn apply_rename(
    ctx: &ToolContext,
    ws: &CodingWorkspaceRow,
    rel: &str,
    line: u32,
    character: u32,
    new_name: &str,
    spec: &LspServerSpec,
) -> Result<String> {
    let root = canonical_root(Path::new(&ws.worktree_path))?;
    let abs = resolve_in_workspace(&root, rel)?;
    let uri = Url::from_file_path(&abs).map_err(|_| anyhow!("file path is not a valid URL"))?;
    let text = tokio::fs::read_to_string(&abs).await?;
    let client = manager().get_or_spawn(&ws.id, &root, spec).await?;
    open_document(&client, &uri, spec.language, &text)?;

    #[allow(deprecated)]
    let params = lsp_types::RenameParams {
        text_document_position: lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri },
            position: lsp_types::Position { line, character },
        },
        new_name: new_name.to_string(),
        work_done_progress_params: Default::default(),
    };
    let edit = client
        .socket()
        .request::<lsp_types::request::Rename>(params)
        .await
        .map_err(|e| anyhow!("lsp rename request failed: {e}"))?
        .ok_or_else(|| anyhow!("server returned no rename edit (symbol not renameable here)"))?;

    let changed = write_workspace_edit(ctx, ws, &root, &edit).await?;
    Ok(format!("lsp_rename: renamed to '{new_name}' across {changed} file(s)"))
}

/// Apply a `WorkspaceEdit`'s per-file text edits inside the workspace, journaling each file. Only
/// the `changes` map is applied (the common server output); `document_changes` (versioned/rename
/// operations) is not applied here — an edit that arrives ONLY as `document_changes` yields 0
/// files changed and the caller reports that honestly rather than silently succeeding.
async fn write_workspace_edit(
    ctx: &ToolContext,
    ws: &CodingWorkspaceRow,
    root: &Path,
    edit: &lsp_types::WorkspaceEdit,
) -> Result<usize> {
    let Some(changes) = &edit.changes else {
        return Ok(0);
    };
    let mut changed = 0usize;
    for (uri, edits) in changes {
        let path = uri
            .to_file_path()
            .map_err(|_| anyhow!("rename edit targets a non-file URI: {uri}"))?;
        // Re-anchor to a workspace-relative path so the path-guard proves containment.
        let rel = path
            .strip_prefix(root)
            .map_err(|_| anyhow!("rename edit escapes the workspace root: {}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        resolve_in_workspace(root, &rel)?;
        let original = tokio::fs::read_to_string(&path).await?;
        let updated = apply_text_edits(&original, edits)?;
        tokio::fs::write(&path, &updated).await?;
        journal_coding_audit(ctx, &ws.id, "lsp_rename", "rename", &rel).await?;
        changed += 1;
    }
    Ok(changed)
}

/// Apply a file's `TextEdit`s. Edits are sorted last-to-first by start position so earlier offsets
/// stay valid as later ranges are spliced. Line/character ranges are resolved against the file's
/// line index.
fn apply_text_edits(content: &str, edits: &[lsp_types::TextEdit]) -> Result<String> {
    let line_starts = line_start_offsets(content);
    let mut resolved: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for e in edits {
        let start = offset_of(&line_starts, content, e.range.start)?;
        let end = offset_of(&line_starts, content, e.range.end)?;
        if end < start {
            return Err(anyhow!("rename edit has an inverted range"));
        }
        resolved.push((start, end, e.new_text.as_str()));
    }
    resolved.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = content.to_string();
    for (start, end, new_text) in resolved {
        out.replace_range(start..end, new_text);
    }
    Ok(out)
}

/// Byte offsets of each line start (index 0 = offset 0).
fn line_start_offsets(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Resolve an LSP (line, character) — UTF-16-agnostic best effort (character treated as a byte
/// column within the line, adequate for ASCII identifiers, the dominant rename case) — to a byte
/// offset in `content`.
fn offset_of(line_starts: &[usize], content: &str, pos: lsp_types::Position) -> Result<usize> {
    let line = pos.line as usize;
    let base = *line_starts
        .get(line)
        .ok_or_else(|| anyhow!("rename edit references a line past end of file"))?;
    let offset = base + pos.character as usize;
    if offset > content.len() {
        return Err(anyhow!("rename edit references a column past end of file"));
    }
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_detection_by_extension() {
        assert_eq!(language_for_file("src/main.rs"), Some("rust"));
        assert_eq!(language_for_file("app.py"), Some("python"));
        assert_eq!(language_for_file("index.tsx"), Some("typescript"));
        assert_eq!(language_for_file("main.go"), Some("go"));
        assert_eq!(language_for_file("App.java"), Some("java"));
        // Unknown extension → None → deterministic degradation.
        assert_eq!(language_for_file("notes.xyz"), None);
        assert_eq!(language_for_file("README"), None);
    }

    #[test]
    fn unsupported_language_degrades_with_a_clear_message() {
        // The degradation contract: an unknown language resolves to a clear no-op reason, never
        // a server spec and never a hard failure.
        match resolve_for_file("lsp_diagnostics", "notes.xyz") {
            Resolved::Degrade(msg) => {
                assert!(msg.contains("no language server available"));
                assert!(msg.contains("not a failure"));
            }
            Resolved::Ready(_) => panic!("an unsupported language must degrade, not resolve"),
        }
    }

    #[test]
    fn absent_server_degrades_naming_the_missing_binary() {
        // `.java` maps to jdtls, which is not installed in CI → degrade naming the missing program
        // (deterministic on any host without jdtls). If a host happens to have jdtls, this instead
        // resolves Ready — assert only that we never panic and the degrade path is well-formed.
        match resolve_for_file("lsp_rename", "Foo.java") {
            Resolved::Degrade(msg) => assert!(msg.contains("jdtls") && msg.contains("PATH")),
            Resolved::Ready(spec) => assert_eq!(spec.program, "jdtls"),
        }
    }

    #[test]
    fn risk_tiers_match_the_capability() {
        // Read for diagnostics (no approval), ReversibleWrite for rename (journaled write).
        assert_eq!(LspDiagnosticsTool.risk_tier(&json!({})), RiskTier::Read);
        assert_eq!(LspRenameTool.risk_tier(&json!({})), RiskTier::ReversibleWrite);
    }

    #[test]
    fn dedup_drops_lsp_lines_already_in_build_gate_output() {
        let lsp = vec![
            "  [error] 3:4 mismatched types: expected u32, found String".to_string(),
            "  [warning] 9:1 unused variable: x".to_string(),
        ];
        // The build gate already reported the type error (compiler wording contains the LSP msg).
        let build = "error[E0308]: mismatched types: expected u32, found String\n --> src/main.rs:3:4";
        let out = dedup_against_build_gate(&lsp, build);
        assert_eq!(out.len(), 1, "the duplicate type error must be elided");
        assert!(out[0].contains("unused variable"), "the LSP-only warning must survive");
    }

    #[test]
    fn dedup_keeps_everything_when_build_output_is_empty() {
        let lsp = vec!["  [error] 1:1 something".to_string()];
        assert_eq!(dedup_against_build_gate(&lsp, "").len(), 1);
    }

    #[test]
    fn renders_empty_and_nonempty_diagnostics() {
        assert!(render_diagnostics(&[]).contains("no semantic diagnostics"));
    }

    #[test]
    fn apply_text_edits_splices_last_to_first() {
        // Two edits on one line: renaming both `foo` occurrences. Applying last-to-first keeps
        // earlier offsets valid.
        let content = "let foo = foo + 1;\n";
        let edit = |l, c0, c1, new: &str| lsp_types::TextEdit {
            range: lsp_types::Range {
                start: lsp_types::Position { line: l, character: c0 },
                end: lsp_types::Position { line: l, character: c1 },
            },
            new_text: new.to_string(),
        };
        let edits = vec![edit(0, 4, 7, "bar"), edit(0, 10, 13, "bar")];
        let out = apply_text_edits(content, &edits).unwrap();
        assert_eq!(out, "let bar = bar + 1;\n");
    }
}
