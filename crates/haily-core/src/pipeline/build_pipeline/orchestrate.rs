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
use haily_db::queries::pipeline_runs;
use haily_db::DbHandle;
use haily_tools::coding::workspace::CodingWorkspace;
use uuid::Uuid;

use super::{
    build_phase_pipeline, build_review_pipeline, detect_test_tampering, ship_pipeline, Finding,
    FindingsDoc, PhaseInput, VerifierCmd, BUILD_DOMAIN, BUILD_SYSTEM_PROMPT, REVIEW_SYSTEM_PROMPT,
    SHIP_SYSTEM_PROMPT,
};
use crate::pipeline::exemplar;
use crate::pipeline::runner::{PipelineRunner, RunReport, RunSpec};
use crate::pipeline::{Pipeline, RunStatus};

use super::diff;

/// The bounded Fix-loop budget (P6: "≤2 rounds"). After this many fix rounds still show an
/// unresolved Critical, the run pauses for the user rather than shipping.
const MAX_FIX_ROUNDS: u32 = 2;

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
            let mut criticals: Vec<Finding> = match read_findings(db, &rev_report.run_id).await {
                Some(findings) => findings.into_iter().filter(|f| f.is_critical()).collect(),
                // No findings recorded (the reviewer never emitted) — can't confirm clean, so
                // conservatively do not ship.
                None => return Ok(report(rev_report.run_id, RunStatus::Paused, total_retries)),
            };
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
            feedback = Some(render_feedback(&criticals));
            round += 1;
        }
    }

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
    RunReport { run_id, status, retries }
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
        RunReport { run_id: "r1".to_string(), status, retries: 0 }
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
}
