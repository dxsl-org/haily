//! `haily eval coding` support (Sub-Agent + Skill Architecture phase 9).
//!
//! Builds the eval dependencies (db, kms, LLM router, tools) from a minimal boot — NOT the full
//! [`crate::AppHandle::bootstrap`], which would spawn daemons — and drives every fixture under
//! `evals/fixtures/` through the coding eval runner, writing a report + `eval_runs` rows.
//!
//! ## Honest deferral (mirrors the P0 spike report)
//! The baseline MATRIX RUN needs a configured local/cloud model host. This build env has none, so
//! the runner refuses to fabricate: with `HAILY_EVAL_MODEL` UNSET it prints guidance and exits
//! cleanly (the matrix is a documented manual step, see docs/project-roadmap.md). With it SET the
//! model name is recorded on every `eval_runs` row and the fixtures actually run.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use haily_core::pipeline::eval_runner::{render_outcome, render_report};
use haily_core::pipeline::{parse_task_yaml, run_coding_eval, EvalDeps, EvalMode, EvalOutcome};
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::LlmRouter;
use haily_types::{DepthMode, Request, RequestOrigin};
use uuid::Uuid;

use crate::config::load_llm_config;

/// Run the coding eval over every fixture under `fixtures_dir` (default `evals/fixtures`).
///
/// `depth_label` is `quick`/`normal`/`deep`; `escalation` selects the P3 `{off,on}` matrix arm.
/// Returns `Ok(())` after writing the report — a failing fixture is a scored outcome, not an error.
///
/// # Errors
/// Returns an error only for a setup failure (DB/KMS open, fixtures dir missing, report write).
pub async fn run_coding_eval_all(
    data_dir: &Path,
    depth_label: &str,
    escalation: bool,
    fixtures_dir: Option<PathBuf>,
) -> Result<()> {
    let fixtures_dir = fixtures_dir.unwrap_or_else(|| PathBuf::from("evals/fixtures"));
    let model = match std::env::var("HAILY_EVAL_MODEL") {
        Ok(m) if !m.trim().is_empty() => m,
        _ => {
            eprintln!(
                "haily eval coding: HAILY_EVAL_MODEL is not set. The coding baseline MATRIX RUN \
                 requires a configured local/cloud model host, which this environment does not \
                 provide. Set HAILY_EVAL_MODEL=<model-name> (and configure the LLM router via \
                 preferences) to run the fixtures. The scripted-LLM pipeline goldens run offline \
                 via `cargo test -p haily-core --test coding_goldens`. See docs/project-roadmap.md \
                 for the manual matrix protocol."
            );
            return Ok(());
        }
    };

    let db = Arc::new(DbHandle::init(&data_dir.join("haily.db")).await.context("open db")?);
    let kms = Arc::new(KmsHandle::init((*db).clone(), data_dir).await.context("init kms")?);
    let cfg = load_llm_config(&kms).await;
    let llm = Arc::new(RwLock::new(Arc::new(LlmRouter::init(cfg).await)));

    let depth = DepthMode::from_label(depth_label);
    let tier_config = if escalation { "local+escalate" } else { "local" };
    let deps = EvalDeps {
        db: Arc::clone(&db),
        kms,
        llm,
        model,
        tier_config: tier_config.to_string(),
        escalation_enabled: escalation,
    };

    // SEC-H: mint the eval-mode witness from a CLI-origin request — a chat request could never
    // reach this. The unwrap is sound: `RequestOrigin::Cli` always yields `Some`.
    let cli_req = Request {
        session_id: Uuid::new_v4(),
        adapter_id: "eval-cli".to_string(),
        message: "eval coding".to_string(),
        user_ref: None,
        depth,
        origin: RequestOrigin::Cli,
    };
    let mode = EvalMode::from_request(&cli_req)
        .context("eval mode requires a CLI-origin request (SEC-H)")?;

    let mut fixtures = discover_fixtures(&fixtures_dir).await?;
    fixtures.sort();
    if fixtures.is_empty() {
        eprintln!("no fixtures found under {}", fixtures_dir.display());
        return Ok(());
    }

    let mut sections = Vec::new();
    let mut passed = 0usize;
    for fixture in &fixtures {
        let manifest_src = tokio::fs::read_to_string(fixture.join("task.yaml"))
            .await
            .with_context(|| format!("reading {}/task.yaml", fixture.display()))?;
        let manifest = match parse_task_yaml(&manifest_src) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("skipping malformed fixture {}: {e:#}", fixture.display());
                continue;
            }
        };
        eprintln!("eval: running {} ({})…", manifest.id, manifest.language);
        match run_coding_eval(&deps, &manifest, fixture, depth, mode).await {
            Ok(outcome) => {
                if outcome.score.passed {
                    passed += 1;
                }
                report_line(&outcome);
                sections.push(render_outcome(&outcome));
            }
            Err(e) => eprintln!("eval: {} FAILED to run: {e:#}", manifest.id),
        }
    }

    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let title = format!("Coding Eval — {ts} (model: {}, depth: {depth_label})", deps.model);
    let report = render_report(&title, &sections);
    let report_dir = PathBuf::from(".agents/reports");
    tokio::fs::create_dir_all(&report_dir).await.ok();
    let report_path = report_dir.join(format!("eval-{ts}.md"));
    tokio::fs::write(&report_path, report).await.context("writing eval report")?;

    eprintln!(
        "eval: {passed}/{} fixtures passed. Report: {}",
        fixtures.len(),
        report_path.display()
    );
    Ok(())
}

fn report_line(o: &EvalOutcome) {
    eprintln!(
        "  {} → {} ({} ms, {} escalations, egress {})",
        o.task_id,
        if o.score.passed { "PASS" } else { "FAIL" },
        o.wall_clock_ms,
        o.escalation_count,
        o.egress_summary()
    );
}

/// A directory is a fixture iff it holds a `task.yaml`.
async fn discover_fixtures(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(out), // missing dir → no fixtures (caller reports)
    };
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.is_dir() && path.join("task.yaml").is_file() {
            out.push(path);
        }
    }
    Ok(out)
}
