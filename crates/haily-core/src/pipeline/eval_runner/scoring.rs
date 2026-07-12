//! Deterministic, gate-based eval scoring (Sub-Agent + Skill Architecture phase 9).
//!
//! Scoring is a PURE function of observed facts — NOT an LLM judge (locked decision: a local
//! model judging its own coding output has self-preference bias). Every gate is a structural
//! check (a command exit code, a filesystem fact, a DB row count), so the same [`ScoreInputs`]
//! always yields byte-identical [`ScoreResult`] JSON — the bit-stability the Router A/B signal
//! and the scripted-suite reproducibility test both depend on.

use serde::{Deserialize, Serialize};

/// One scored gate. `detail` is a short, deterministic explanation (never a timestamp or a
/// nondeterministic path) so the serialized result is reproducible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateResult {
    pub gate: String,
    pub pass: bool,
    pub detail: String,
}

/// The observed facts scoring reduces to a verdict. Deliberately plain data (no handles, no
/// clock) so [`score`] is a pure, reproducible function.
#[derive(Debug, Clone)]
pub struct ScoreInputs {
    /// Exit code of the fixture's own gate command run against the final workspace (`None` when
    /// the verifier toolchain was absent — scored as a fail, an AD-M3 non-pass).
    pub gate_exit: Option<i32>,
    /// Whether the ORIGINAL fixture directory is byte-unchanged (copy-per-run invariant): proof
    /// the eval wrote nothing outside its throwaway workspace.
    pub fixture_original_unchanged: bool,
    /// Number of `pipeline_runs` rows recorded for this eval's session — journal completeness.
    pub journal_rows: usize,
    /// Whether the ship stage applied to the real repo. In eval this MUST be false (worktree_apply
    /// is structurally hard-blocked); a true here is a critical harness breach → automatic fail.
    pub ship_applied: bool,
}

/// The scored verdict: the ordered gate list plus the `passed` roll-up (every gate must pass).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScoreResult {
    pub gates: Vec<GateResult>,
    pub passed: bool,
}

impl ScoreResult {
    /// Canonical JSON of the score — deterministic (serde emits fields in declaration order), so
    /// two runs over identical [`ScoreInputs`] produce byte-identical output. This is the
    /// bit-stable digest the reproducibility test asserts on.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Score an eval run from observed facts. Order is fixed (build/type-check + tests → no
/// out-of-workspace writes → journal complete → ship never applied) so the serialized result is
/// stable. `passed` is the AND of every gate.
pub fn score(inputs: &ScoreInputs) -> ScoreResult {
    let gate_pass = inputs.gate_exit == Some(0);
    let gate_detail = match inputs.gate_exit {
        Some(0) => "gate exit 0".to_string(),
        Some(code) => format!("gate exit {code}"),
        None => "verifier toolchain absent".to_string(),
    };

    let gates = vec![
        GateResult {
            gate: "builds_and_tests_pass".to_string(),
            pass: gate_pass,
            detail: gate_detail,
        },
        GateResult {
            gate: "no_out_of_workspace_writes".to_string(),
            pass: inputs.fixture_original_unchanged,
            detail: if inputs.fixture_original_unchanged {
                "fixture original byte-unchanged".to_string()
            } else {
                "fixture original was modified".to_string()
            },
        },
        GateResult {
            gate: "journal_complete".to_string(),
            pass: inputs.journal_rows > 0,
            detail: format!("{} pipeline_run row(s)", inputs.journal_rows),
        },
        GateResult {
            gate: "ship_not_applied".to_string(),
            // Passing == the ship never touched a real repo (the eval invariant).
            pass: !inputs.ship_applied,
            detail: if inputs.ship_applied {
                "BREACH: ship applied to a real repo".to_string()
            } else {
                "ship hard-blocked (no apply)".to_string()
            },
        },
    ];
    let passed = gates.iter().all(|g| g.pass);
    ScoreResult { gates, passed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_inputs() -> ScoreInputs {
        ScoreInputs {
            gate_exit: Some(0),
            fixture_original_unchanged: true,
            journal_rows: 3,
            ship_applied: false,
        }
    }

    #[test]
    fn all_gates_pass_yields_passed() {
        let r = score(&clean_inputs());
        assert!(r.passed);
        assert_eq!(r.gates.len(), 4);
        assert!(r.gates.iter().all(|g| g.pass));
    }

    #[test]
    fn a_failing_gate_command_fails_the_run() {
        let mut i = clean_inputs();
        i.gate_exit = Some(101);
        let r = score(&i);
        assert!(!r.passed, "a nonzero gate exit must fail the run");
        assert!(!r.gates[0].pass);
    }

    #[test]
    fn absent_verifier_is_a_fail_not_a_pass() {
        let mut i = clean_inputs();
        i.gate_exit = None;
        let r = score(&i);
        assert!(!r.passed, "an absent verifier toolchain is scored as a fail (AD-M3)");
        assert!(r.gates[0].detail.contains("absent"));
    }

    #[test]
    fn a_ship_apply_is_a_critical_breach_fail() {
        let mut i = clean_inputs();
        i.ship_applied = true;
        let r = score(&i);
        assert!(!r.passed, "a ship apply in eval must fail the run");
        assert!(r.gates.iter().any(|g| g.gate == "ship_not_applied" && !g.pass));
    }

    #[test]
    fn scoring_is_bit_stable_across_runs() {
        // CRITICAL (reproducibility): identical inputs → byte-identical serialized score.
        let a = score(&clean_inputs()).to_json();
        let b = score(&clean_inputs()).to_json();
        assert_eq!(a, b, "scoring must be reproducible/bit-stable for the scripted suite");
        // And a different input set differs (the digest actually discriminates).
        let mut other = clean_inputs();
        other.gate_exit = Some(1);
        assert_ne!(a, score(&other).to_json());
    }
}
