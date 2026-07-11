//! Eval fixture task manifest (`task.yaml`) — the self-describing contract each fixture repo
//! carries (Sub-Agent + Skill Architecture phase 9).
//!
//! The manifest is authored BY the fixtures (never user-supplied), and the P9 schema is a FLAT
//! scalar set + one string list, so this parses that exact subset directly rather than pulling
//! in a general-YAML dependency (KISS — no `serde_yaml`/`serde_yml`). Block scalars, anchors,
//! and flow collections are intentionally NOT supported; fixtures use single-line quoted values.

use anyhow::{bail, Context, Result};

use crate::pipeline::build_pipeline::VerifierCmd;

/// A parsed fixture task manifest. The `gate` command is the deterministic, language-native
/// pass condition (NOT an LLM judge — locked decision): exit 0 == pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskManifest {
    /// Fixture id (e.g. `rust-fix-compile`) — the `eval_runs.task_id`.
    pub id: String,
    /// `rust` | `typescript` | `python` | `go` — proves the language-agnostic gate beyond Rust.
    pub language: String,
    /// `fix-compile-error` | `fix-failing-test` | `feature-with-tests` | `refactor-rename`.
    pub kind: String,
    /// The task the model is given (single line).
    pub description: String,
    /// The gate command line (e.g. `cargo test`, `pytest -q`, `go test ./...`, `npm test`).
    pub gate: String,
    pub max_tool_calls: u32,
    pub max_escalations: u32,
    pub timeout_seconds: u64,
    /// `Some("hard")` marks a fixture KNOWN to fail single-pass on a weak model (calibration —
    /// prevents ceiling effects that would mask model differences).
    pub calibration: Option<String>,
    /// Invariants the run must not violate (audit/report only; the structural guards enforce them).
    pub invariants: Vec<String>,
}

impl TaskManifest {
    /// The gate command split into a [`VerifierCmd`] (program + args on whitespace). The gate is
    /// developer-authored (fixture-owned), never LLM-chosen, so a plain whitespace split is safe.
    ///
    /// # Errors
    /// Returns an error if the gate command is empty.
    pub fn gate_cmd(&self) -> Result<VerifierCmd> {
        let mut parts = self.gate.split_whitespace();
        let program = parts.next().context("task.yaml `gate` command is empty")?;
        let args: Vec<&str> = parts.collect();
        Ok(VerifierCmd::new(program, &args))
    }
}

/// Parse the P9 `task.yaml` subset. Fail-loud on a missing required field (a malformed fixture
/// is a bug to surface, never a silently-skipped eval).
///
/// # Errors
/// Returns an error if a required field is absent or a numeric field does not parse.
pub fn parse_task_yaml(src: &str) -> Result<TaskManifest> {
    let mut id = None;
    let mut language = None;
    let mut kind = None;
    let mut description = None;
    let mut gate = None;
    let mut max_tool_calls = None;
    let mut max_escalations = None;
    let mut timeout_seconds = None;
    let mut calibration = None;
    let mut invariants = Vec::new();
    let mut in_invariants = false;

    for raw in src.lines() {
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            continue;
        }
        // A list item under `invariants:` — only while that block is open.
        if let Some(item) = line.trim_start().strip_prefix("- ") {
            if in_invariants {
                invariants.push(unquote(item.trim()));
            }
            continue;
        }
        // Any non-indented `key:` line closes an open list block.
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            in_invariants = false;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        match key {
            "id" => id = Some(value),
            "language" => language = Some(value),
            "kind" => kind = Some(value),
            "description" => description = Some(value),
            "gate" => gate = Some(value),
            "max_tool_calls" => max_tool_calls = Some(parse_num(&value, key)?),
            "max_escalations" => max_escalations = Some(parse_num(&value, key)?),
            "timeout_seconds" => timeout_seconds = Some(parse_num(&value, key)?),
            "calibration" => calibration = Some(value),
            "invariants" => in_invariants = true,
            _ => {}
        }
    }

    Ok(TaskManifest {
        id: required(id, "id")?,
        language: required(language, "language")?,
        kind: required(kind, "kind")?,
        description: required(description, "description")?,
        gate: required(gate, "gate")?,
        max_tool_calls: required(max_tool_calls, "max_tool_calls")?,
        max_escalations: required(max_escalations, "max_escalations")?,
        timeout_seconds: required(timeout_seconds, "timeout_seconds")?,
        calibration: calibration.filter(|c| !c.is_empty()),
        invariants,
    })
}

fn strip_comment(line: &str) -> &str {
    // A `#` only starts a comment when not inside a quoted value; fixtures never quote a `#`, so
    // a plain split is sufficient for this authored subset.
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn parse_num<T: std::str::FromStr>(v: &str, key: &str) -> Result<T> {
    v.parse::<T>()
        .map_err(|_| anyhow::anyhow!("task.yaml `{key}` is not a valid number: {v:?}"))
}

fn required<T>(v: Option<T>, key: &str) -> Result<T> {
    match v {
        Some(v) => Ok(v),
        None => bail!("task.yaml missing required field `{key}`"),
    }
}
