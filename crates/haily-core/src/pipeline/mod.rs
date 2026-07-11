//! Pipeline engine — the Rust-native stage machine that converts a weak model into
//! HailyKit-class output by keeping sequencing, gating, and retry as deterministic code (the
//! LLM only fills bounded stages).
//!
//! This module (phase 4a) carries the DECLARATIVE types + pure helpers:
//! - [`stage`]: [`Stage`], [`Pipeline`], the goclaw-style [`StageOutcome`] exit codes, and
//!   [`RunStatus`] (typed mirror of `pipeline_runs.status`).
//! - [`gate`]: [`Gate`] (command / artifact / approval) + [`ArtifactKind`] parse helpers.
//! - [`verifier_output`]: the language-agnostic decisive-output parser for command gates.
//!
//! The RUNNER (sequential execution, retry loop, escalation, pause/resume, worktree
//! compensation, RunEvent emission, and the shared-handle threading that keeps the pipeline an
//! orthogonal orchestration axis rather than a delegation level) lands in phase 4b — it is the
//! agent-loop-touching half and is intentionally NOT in this PR.

pub mod gate;
pub mod stage;
pub mod verifier_output;

pub use gate::{ArtifactKind, Gate};
pub use stage::{Pipeline, RunStatus, Stage, StageOutcome, DEFAULT_MAX_TOOL_CALLS};
pub use verifier_output::{parse_decisive, VerifierLang};
