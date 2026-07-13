//! The Build Pipeline wrapper — the Fix loop, Critical-finding routing, reward-hacking guard,
//! and honest negative-result that the P4 runner does not have natively (it retries the FAILING
//! stage; a Fix must re-run an EARLIER stage). Mirrors P5's `run_plan`: composition-driven, it
//! calls `runner.run` for each per-phase Pipeline and orchestrates across those runs.
//!
//! Per phase: Build+Test (one run) → Review (a second run, so the post-build diff can be
//! injected as data) → decide. A Critical finding (model-reported OR the reward-hacking guard's
//! synthetic one) re-runs Build+Test with the findings as feedback, ≤[`MAX_FIX_ROUNDS`] times.
//! Unresolved after the bound → a Paused report, nothing shipped (negative-results honesty —
//! never mask failure as success). All phases clean → the whole-plan Ship run.

use anyhow::Result;
use haily_db::queries::review_findings::{self, NewReviewFinding};
use haily_db::queries::{meta, pipeline_runs};
use haily_db::DbHandle;
use haily_kms::distillation;
use haily_tools::coding::workspace::CodingWorkspace;
use haily_types::{DepthMode, Notification};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{
    build_phase_pipeline, build_review_pipeline, detect_test_tampering, ship_pipeline, Finding,
    FindingsDoc, PhaseInput, Severity, VerifierCmd, BUILD_DOMAIN, BUILD_SYSTEM_PROMPT,
    REVIEW_SYSTEM_PROMPT, SHIP_SYSTEM_PROMPT,
};
use crate::pipeline::exemplar;
use crate::pipeline::runner::{PipelineRunner, RunReport, RunSpec};
use crate::pipeline::{Pipeline, RunStatus};

use super::diff;

/// The bounded Fix-loop budget (P6: "≤2 rounds"). After this many fix rounds still show an
/// unresolved Critical, the run pauses for the user rather than shipping.
const MAX_FIX_ROUNDS: u32 = 2;

/// A `(category, module)` class must recur at least this many times ACROSS runs before it
/// yields a distillation proposal at Ship (phase 8).
const DISTILLATION_MIN_RECURRENCE: i64 = 2;
/// Days a proposed class stays on cooldown — a dismissed (or approved) proposal does not
/// re-fire for the same class within this window (phase 8 anti-nag).
const DISTILLATION_COOLDOWN_DAYS: i64 = 7;
/// Max distinct finding summaries itemized into one proposal (keeps the card readable).
const DISTILLATION_MAX_RULES: i64 = 5;

/// Inputs for [`run_build`], grouped to keep its arity sane (mirrors `PlanRunSpec`).
pub struct BuildRunSpec<'a> {
    /// Plan phases in dependency order (sequential; parallel deferred).
    pub phases: Vec<PhaseInput>,
    pub session_id: Uuid,
    pub work_item_id: Option<String>,
    /// Seeds each per-phase run's persistent liveness counter (FMA-C1).
    pub attempts_budget: i64,
    pub workspace: &'a CodingWorkspace,
    /// Compile gate command (developer-authored, sandbox-run).
    pub compile: VerifierCmd,
    /// Test gate command.
    pub test: VerifierCmd,
    /// Judgment depth (phase 7): `Deep` runs refuter votes on each model-reported Critical
    /// finding before routing it into the Fix loop (a majority-refuted false positive is
    /// dropped); `Normal`/`Quick` route every Critical straight to the loop. The
    /// reward-hacking guard's synthetic Critical is deterministic and NEVER refuted away.
    pub depth: DepthMode,
    /// Phase 8 (DEP-C2) — the seam a recurring-findings distillation PROPOSAL is emitted on at
    /// Ship. `None` disables emission (the proposal is still cooldown-tracked but not surfaced).
    /// A `haily-types::Notification` mpsc, NOT a direct `haily-io` dependency (haily-core never
    /// imports haily-io); the app layer bridges this to `AdapterManager::notify_all`.
    pub distillation_tx: Option<mpsc::Sender<Notification>>,
}

impl<'a> BuildRunSpec<'a> {
    fn run_spec(
        &self,
        pipeline: Pipeline,
        system_prompt: &'static str,
        domain_name: &'static str,
    ) -> RunSpec<'a> {
        RunSpec {
            pipeline,
            session_id: self.session_id,
            work_item_id: self.work_item_id.clone(),
            system_prompt,
            domain_name,
            attempts_budget: self.attempts_budget,
            workspace: self.workspace,
        }
    }
}

/// Drive the whole plan to a shipped, paused, or failed state.
///
/// Returns the terminal [`RunReport`] of the last run driven: the Ship run on success, or the
/// run that could not pass (build/test) / could not be confirmed clean (unresolved Critical) as
/// a `Paused`/failed report — nothing is shipped in either case.
///
/// # Errors
/// Returns an error only for a runner SETUP failure (see [`PipelineRunner::run`]); a paused or
/// failed phase is a normal outcome on the report, not an error.
pub async fn run_build(
    runner: &PipelineRunner,
    db: &DbHandle,
    spec: BuildRunSpec<'_>,
) -> Result<RunReport> {
    let mut total_retries: u32 = 0;

    // Phase 7 parity hint (text-only) at pipeline start — never blocks/escalates.
    runner.emit_parity_hint(spec.depth).await;

    for phase in &spec.phases {
        // Exemplars: same-extension recent neighbors, excluding the phase's own target files.
        let ext = exemplar::primary_ext(&phase.target_files).unwrap_or_default();
        let exemplar_block =
            exemplar::build_exemplar_block(spec.workspace.worktree_root(), &ext, &phase.target_files)
                .await;
        // The commit before this phase's first build — the base for the phase diff shown to Review.
        let phase_base = diff::head_sha(spec.workspace).await;

        let mut feedback: Option<String> = None;
        let mut round: u32 = 0;
        loop {
            let round_base = diff::head_sha(spec.workspace).await;

            // Build + Test (verifier-grounded retry is the runner's job inside this run).
            let bt = build_phase_pipeline(
                phase,
                &exemplar_block,
                &spec.compile,
                &spec.test,
                feedback.as_deref(),
            );
            let bt_report =
                runner.run(spec.run_spec(bt, BUILD_SYSTEM_PROMPT, BUILD_DOMAIN)).await?;
            total_retries += bt_report.retries;
            if bt_report.status != RunStatus::Done {
                // Compile/Test could not pass — honest failure, nothing shipped.
                return Ok(report(bt_report.run_id, bt_report.status, total_retries));
            }

            // Review runs even when the gates pass (P6): a SECOND run so the post-build diff is
            // injected into the distinct reviewer sub-turn (reviewer ≠ builder).
            let phase_diff = diff::diff_since(spec.workspace, phase_base.as_deref()).await;
            let review = build_review_pipeline(phase, &phase_diff);
            let rev_report =
                runner.run(spec.run_spec(review, REVIEW_SYSTEM_PROMPT, BUILD_DOMAIN)).await?;
            total_retries += rev_report.retries;
            if rev_report.status != RunStatus::Done {
                return Ok(report(rev_report.run_id, RunStatus::Paused, total_retries));
            }

            // Collect unresolved Criticals: the reviewer's own + the reward-hacking guard's
            // synthetic one (only on a Fix round — a phase legitimately adding tests in round 0
            // is not tampering; a fix that weakens a test to go green is).
            let all_findings = match read_findings(db, &rev_report.run_id).await {
                Some(findings) => findings,
                // No findings recorded (the reviewer never emitted) — can't confirm clean, so
                // conservatively do not ship.
                None => return Ok(report(rev_report.run_id, RunStatus::Paused, total_retries)),
            };
            // Phase 8: persist every finding to the cross-run history so the Ship recurrence
            // detector can spot a class that keeps coming back (best-effort — never blocks build).
            persist_review_findings(db, &rev_report.run_id, &spec, &all_findings).await;
            let mut criticals: Vec<Finding> =
                all_findings.into_iter().filter(|f| f.is_critical()).collect();
            // Deep: run refuter votes on each model-reported Critical. A finding survives on at
            // least one non-refutation (uncertainty defaults to NOT refuted, so it stands) and
            // is dropped only when a majority of refuters confidently refute it — cutting the
            // reviewer's false positives without ever silencing a genuine bug. Applied ONLY to
            // model-reported findings; the deterministic reward-hacking guard below is never
            // subject to a vote.
            if spec.depth == DepthMode::Deep && !criticals.is_empty() {
                let jc = runner.judge_context(spec.session_id);
                let mut survivors = Vec::new();
                for f in criticals {
                    let evidence = format!("{} {}", f.file, f.failure_scenario);
                    if crate::pipeline::judge::refuter_votes(&jc, &f.summary, &evidence).await {
                        survivors.push(f);
                    }
                }
                criticals = survivors;
            }
            if round > 0 {
                let fix_delta = diff::diff_since(spec.workspace, round_base.as_deref()).await;
                if let Some(f) = detect_test_tampering(&fix_delta) {
                    criticals.push(f);
                }
            }

            if criticals.is_empty() {
                break; // phase built + reviewed clean → next phase
            }
            if round >= MAX_FIX_ROUNDS {
                // Unresolved Critical after the bounded Fix loop — pause, never mask as success.
                return Ok(report(rev_report.run_id, RunStatus::Paused, total_retries));
            }
            // Phase 10 seam finally wired (pipeline-activation phase 4): fold semantic LSP
            // diagnostics for the phase's changed files into the Fix-loop feedback, deduplicated
            // against THIS round's own compile-gate output so the model never sees the same
            // error twice. Best-effort — a missing/degraded server yields no lines and the round
            // proceeds on reviewer findings alone.
            let lsp_lines =
                lsp_fix_signal(spec.workspace, &phase.target_files, &bt_report.last_gate_output)
                    .await;
            feedback = Some(append_lsp_signal(render_feedback(&criticals), &lsp_lines));
            round += 1;
        }
    }

    // Phase 8: recurrence detector runs at Ship — a class of finding that recurred across runs
    // becomes a user-approved distillation proposal (proposal-only; never a silent write).
    emit_distillation_proposals(db, &spec).await;

    // Every phase built + reviewed clean → the ONLY write to the real repo.
    let summary = ship_summary(&spec.phases);
    let ship = ship_pipeline(&summary);
    let ship_report = runner.run(spec.run_spec(ship, SHIP_SYSTEM_PROMPT, BUILD_DOMAIN)).await?;
    total_retries += ship_report.retries;
    let ship_report = finalize_ship_report(ship_report, spec.workspace);
    Ok(report(ship_report.run_id, ship_report.status, total_retries))
}

/// Filesystem evidence that `worktree_apply` actually completed. A successful apply
/// (`confirm=true` with a non-empty diff) removes the ephemeral worktree directory via
/// `git worktree remove --force` as its final step (`WorktreeApplyTool::execute`) — the ONLY
/// thing in the Ship stage's tool whitelist (`worktree_apply`/`git_commit`) that can make the
/// workspace's own worktree root vanish. Its absence is proof nothing was applied.
fn apply_evidence_exists(workspace: &CodingWorkspace) -> bool {
    !workspace.worktree_root().is_dir()
}

/// Review fix (P6, HIGH — false "shipped" success): the Ship stage's `Gate::Approval` only
/// proves the USER agreed to proceed — it says nothing about whether the model actually called
/// `worktree_apply`. A weak model that emits prose with no tool call would previously pass the
/// gate and report `Done` while the real repo was never touched, exactly the "declare success
/// without the outcome" failure this whole phase exists to prevent.
///
/// This overrides a `Done` ship report with `Paused` when there is no filesystem evidence the
/// apply ran (see [`apply_evidence_exists`]) — honest failure, never a silently masked success.
/// Any other status (`Paused` from a declined approval, `Failed`/`Interrupted` from a gate
/// error) already reflects a non-ship outcome and passes through unchanged.
fn finalize_ship_report(report: RunReport, workspace: &CodingWorkspace) -> RunReport {
    if report.status == RunStatus::Done && !apply_evidence_exists(workspace) {
        tracing::warn!(
            run = %report.run_id,
            "ship stage completed with no evidence worktree_apply ran — downgrading to paused"
        );
        return RunReport { status: RunStatus::Paused, ..report };
    }
    report
}

/// Read the findings array persisted on a review run's row. `None` when the row is gone or the
/// reviewer emitted nothing (an un-set `findings` column).
async fn read_findings(db: &DbHandle, run_id: &str) -> Option<Vec<Finding>> {
    let row = pipeline_runs::get(db, run_id).await.ok().flatten()?;
    let raw = row.findings?;
    let doc: FindingsDoc = serde_json::from_str(&raw).ok()?;
    Some(doc.findings)
}

/// Render unresolved Critical findings as inert Fix-loop feedback (tag-stripping happens again
/// at the Build-prompt injection site, so this is plain framing).
fn render_feedback(criticals: &[Finding]) -> String {
    let mut s = String::from("The independent review found unresolved CRITICAL issues:");
    for (i, f) in criticals.iter().enumerate() {
        let loc = match (f.file.is_empty(), f.line) {
            (false, Some(l)) => format!(" [{}:{}]", f.file, l),
            (false, None) => format!(" [{}]", f.file),
            _ => String::new(),
        };
        s.push_str(&format!("\n{}.{} {}", i + 1, loc, f.summary));
        if !f.failure_scenario.trim().is_empty() {
            s.push_str(&format!(" — failure: {}", f.failure_scenario.trim()));
        }
    }
    s
}

/// One-line-per-phase completion summary for the Ship stage.
fn ship_summary(phases: &[PhaseInput]) -> String {
    let mut s = format!("Completed {} phase(s):", phases.len());
    for p in phases {
        s.push_str(&format!("\n- {}", p.name));
    }
    s
}

fn report(run_id: String, status: RunStatus, retries: u32) -> RunReport {
    // A synthetic terminal report for an early-exit path (build/test failed, review paused, or
    // the fix loop is unresolved) — there is no further round to enrich, so the gate-output
    // signal is irrelevant here (unlike the `RunReport` the runner itself returns).
    RunReport { run_id, status, retries, last_gate_output: String::new() }
}

/// Best-effort LSP-only semantic-diagnostic lines for the phase's changed files, deduplicated
/// against `build_output` (this round's own compile-gate decisive text) so a diagnostic the gate
/// already reported is never shown to the model twice (phase 4, pipeline-activation, closing the
/// `dedup_against_build_gate` seam). Scoped to `target_files` — never a whole-repo scan.
///
/// Graceful degradation is mandatory: no language server / a spawn or handshake error for a file
/// yields no lines for it (see `haily_tools::lsp::collect_diagnostic_lines`) — this function can
/// never fail the Fix loop, only enrich it when a server happens to be available.
async fn lsp_fix_signal(
    workspace: &CodingWorkspace,
    target_files: &[String],
    build_output: &str,
) -> Vec<String> {
    let lines = haily_tools::lsp::collect_diagnostic_lines(&workspace.row, target_files).await;
    haily_tools::lsp::dedup_against_build_gate(&lines, build_output)
}

/// Append deduped LSP lines to the reviewer-findings feedback, or return `base` unchanged when
/// there is nothing to add (no server / no extra diagnostics) — the degradation path is a plain
/// no-op, never a placeholder header with an empty body.
fn append_lsp_signal(base: String, lsp_lines: &[String]) -> String {
    if lsp_lines.is_empty() {
        return base;
    }
    let mut s = base;
    s.push_str(
        "\n\nAdditional semantic diagnostics from the language server (not already reported by \
         the build gate above):\n",
    );
    s.push_str(&lsp_lines.join("\n"));
    s
}

/// The `category` half of a class key = the finding's severity string (phase 8: start coarse).
fn severity_category(sev: Severity) -> &'static str {
    match sev {
        Severity::Critical => "critical",
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low => "low",
        Severity::Info => "info",
    }
}

/// Persist a review run's findings to the cross-run `review_findings` history (phase 8). Summaries
/// are tag-stripped before storage (they later render into proposals → prompts). Best-effort: a
/// write failure is logged, never fatal to the build.
async fn persist_review_findings(
    db: &DbHandle,
    run_id: &str,
    spec: &BuildRunSpec<'_>,
    findings: &[Finding],
) {
    let session = spec.session_id.to_string();
    let workspace_id = spec.workspace.row.id.clone();
    for f in findings {
        let category = severity_category(f.severity);
        let module = distillation::module_key(&f.file);
        let summary = crate::tool_call::strip_tool_tags(&f.summary);
        let new = NewReviewFinding {
            run_id,
            session_id: &session,
            workspace_id: Some(&workspace_id),
            category,
            module: &module,
            severity: category,
            file: &f.file,
            summary: &summary,
        };
        if let Err(e) = review_findings::insert_finding(db, new).await {
            tracing::warn!(run = %run_id, "review-finding history insert failed: {e:#}");
        }
    }
}

/// Preference key namespacing a class's distillation cooldown marker.
fn cooldown_key(class_key: &str) -> String {
    format!("distillation.cooldown.{class_key}")
}

/// Whether `class_key` is still within its post-proposal cooldown window (phase 8 anti-nag):
/// a class proposed within [`DISTILLATION_COOLDOWN_DAYS`] does not re-fire. Fail-open (treated
/// as NOT on cooldown) if the marker read fails — surfacing a duplicate proposal is a lesser
/// harm than silently swallowing a real recurring finding.
async fn on_cooldown(db: &DbHandle, class_key: &str) -> bool {
    let Ok(Some(last)) = meta::get_preference(db, &cooldown_key(class_key)).await else {
        return false;
    };
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(DISTILLATION_COOLDOWN_DAYS)).to_rfc3339();
    // A marker NEWER than the cutoff means we proposed recently → still on cooldown.
    last.as_str() > cutoff.as_str()
}

/// The Ship-time recurrence detector (phase 8): for each `(category, module)` class that recurred
/// ≥[`DISTILLATION_MIN_RECURRENCE`] times across runs and is not on cooldown, build an itemized
/// proposal, emit it as a [`Notification::DistillationProposal`] (proposal-only), and set the
/// cooldown marker. Entirely best-effort — a DB failure never affects the build outcome.
async fn emit_distillation_proposals(db: &DbHandle, spec: &BuildRunSpec<'_>) {
    let workspace_id = &spec.workspace.row.id;
    let classes = match review_findings::recurrent_classes_for_workspace(
        db,
        workspace_id,
        DISTILLATION_MIN_RECURRENCE,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("distillation recurrence query failed: {e:#}");
            return;
        }
    };

    for class in classes {
        let ck = distillation::class_key(&class.category, &class.module);
        if on_cooldown(db, &ck).await {
            continue;
        }
        let rows = review_findings::findings_for_class(
            db,
            workspace_id,
            &class.category,
            &class.module,
            DISTILLATION_MAX_RULES,
        )
        .await
        .unwrap_or_default();
        // Summaries are already tag-stripped in `persist_review_findings`; strip again defensively
        // (a stored row could predate that guarantee) before it renders into a card.
        let summaries: Vec<String> = rows
            .iter()
            .map(|r| crate::tool_call::strip_tool_tags(&r.summary))
            .collect();
        let proposal =
            distillation::build_proposal(&class.category, &class.module, class.count, &summaries);
        if proposal.rules.is_empty() {
            continue;
        }
        let summary = crate::tool_call::strip_tool_tags(&distillation::render_proposal(&proposal));
        if let Some(tx) = &spec.distillation_tx {
            let _ = tx
                .send(Notification::DistillationProposal {
                    class_key: ck.clone(),
                    summary,
                    rule_count: proposal.rules.len() as u32,
                })
                .await;
        }
        // Mark cooldown whether or not a sender was wired — a proposal was decided for this class.
        if let Err(e) =
            meta::upsert_preference(db, &cooldown_key(&ck), &chrono::Utc::now().to_rfc3339(), "system").await
        {
            tracing::warn!("distillation cooldown marker write failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::DbHandle;

    async fn git(dir: &std::path::Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    async fn workspace_fixture() -> (CodingWorkspace, Vec<tempfile::TempDir>) {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "main"]).await;
        git(repo.path(), &["config", "user.email", "t@haily.test"]).await;
        git(repo.path(), &["config", "user.name", "Test"]).await;
        tokio::fs::write(repo.path().join("README.md"), "hello\n").await.unwrap();
        git(repo.path(), &["add", "."]).await;
        git(repo.path(), &["commit", "-m", "init"]).await;

        let dbdir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dbdir.path().join("t.db")).await.unwrap();
        let session_id = Uuid::new_v4();
        haily_db::queries::sessions::create_session(&db, &session_id.to_string(), "pipeline", None)
            .await
            .unwrap();

        let wt_root = tempfile::tempdir().unwrap();
        let ws = CodingWorkspace::open(&db, &session_id.to_string(), repo.path(), wt_root.path(), None)
            .await
            .expect("open workspace");
        (ws, vec![repo, dbdir, wt_root])
    }

    fn rr(status: RunStatus) -> RunReport {
        RunReport { run_id: "r1".to_string(), status, retries: 0, last_gate_output: String::new() }
    }

    #[tokio::test]
    async fn no_apply_evidence_downgrades_done_to_paused() {
        let (ws, _dirs) = workspace_fixture().await;
        assert!(!apply_evidence_exists(&ws), "an intact worktree is NOT evidence of a completed apply");
        let out = finalize_ship_report(rr(RunStatus::Done), &ws);
        assert_eq!(
            out.status,
            RunStatus::Paused,
            "a Done ship report with no apply evidence must be downgraded, never left as Done"
        );
    }

    #[tokio::test]
    async fn apply_evidence_present_keeps_done() {
        let (ws, _dirs) = workspace_fixture().await;
        // Simulate a completed worktree_apply: it removes the ephemeral worktree directory as
        // its final step.
        tokio::fs::remove_dir_all(ws.worktree_root()).await.unwrap();
        assert!(apply_evidence_exists(&ws), "a removed worktree root IS evidence of a completed apply");
        let out = finalize_ship_report(rr(RunStatus::Done), &ws);
        assert_eq!(out.status, RunStatus::Done, "real evidence must not be second-guessed");
    }

    #[tokio::test]
    async fn a_non_done_status_passes_through_unchanged_regardless_of_evidence() {
        let (ws, _dirs) = workspace_fixture().await;
        for status in [RunStatus::Paused, RunStatus::Failed, RunStatus::Interrupted] {
            let out = finalize_ship_report(rr(status), &ws);
            assert_eq!(out.status, status, "an already-honest non-Done outcome must never be rewritten");
        }
    }

    // -----------------------------------------------------------------------
    // Phase 4 (pipeline-activation): LSP diagnostics folded into the Fix loop.
    // -----------------------------------------------------------------------

    #[test]
    fn append_lsp_signal_surfaces_lsp_only_lines_and_drops_build_gate_duplicates() {
        // Simulates this round's compile-gate output and the lines `collect_diagnostic_lines`
        // would have returned — proves the dedup + merge wiring end to end without a live server.
        let build_output =
            "error[E0308]: mismatched types: expected u32, found String\n --> src/a.rs:3:4";
        let lsp_lines = vec![
            "  [error] 3:4 mismatched types: expected u32, found String".to_string(), // gate dup
            "  [warning] 9:1 unused variable: x".to_string(),                         // LSP-only
        ];
        let deduped = haily_tools::lsp::dedup_against_build_gate(&lsp_lines, build_output);
        let base = "The independent review found unresolved CRITICAL issues:\n1. bug".to_string();
        let feedback = append_lsp_signal(base.clone(), &deduped);

        assert!(
            feedback.contains("unused variable: x"),
            "the LSP-only diagnostic must reach the fix feedback"
        );
        assert!(
            !feedback.contains("mismatched types"),
            "a diagnostic the build gate already reported must be dropped, not duplicated"
        );
        assert!(feedback.starts_with(&base), "reviewer findings stay first; LSP lines are additive");
    }

    #[test]
    fn append_lsp_signal_is_a_no_op_when_there_is_nothing_to_add() {
        let base = "The independent review found unresolved CRITICAL issues:\n1. bug".to_string();
        let feedback = append_lsp_signal(base.clone(), &[]);
        assert_eq!(feedback, base, "no LSP lines means the fix feedback is left completely unaffected");
    }

    #[tokio::test]
    async fn lsp_fix_signal_degrades_cleanly_with_no_server_available() {
        // An unsupported-language file (no server can ever be mapped to it) proves the
        // absent-server path through the REAL async collector, without requiring an actual
        // language server binary on the test host — the round must be unaffected, never fail.
        let (ws, _dirs) = workspace_fixture().await;
        let lines = lsp_fix_signal(&ws, &["README.md".to_string()], "irrelevant build output").await;
        assert!(
            lines.is_empty(),
            "no server for the file's language must yield zero lines, never fail the round"
        );
    }
}
