/// Main agent turn: user message → LLM → tool loop → final response.
///
/// Split into focused submodules (behavior-preserving refactor of the former
/// single-file `agent.rs`): `outcome` (trace/EMA telemetry shared by both turn
/// types), `stream` (streaming hold-back consumer), `sub_turn` (stateless
/// sub-agent turns), `turn` (the full L0 turn). Every item other crates import
/// from `haily_core::agent::*` is re-exported here at the same path it occupied
/// when this was a single file.
mod outcome;
mod stream;
mod sub_turn;
mod turn;

pub use sub_turn::{run_sub_turn, SubTurnRequest};
pub use turn::{run_turn, TurnRuntime};
