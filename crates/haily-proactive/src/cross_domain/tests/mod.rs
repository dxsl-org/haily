//! Tests for cross-domain nudges (Phase 4, "assistant-depth").
//!
//! Lives as an in-crate `#[cfg(test)]` submodule rather than `tests/cross_domain_nudges.rs`
//! (an external integration test) because `lib.rs` declares `mod cross_domain;` as
//! PRIVATE (owned by a different phase, out of scope here) — Rust module privacy
//! requires every path segment to be public for an external crate to name it, so no
//! visibility on `run_tick` itself could make it reachable from `tests/*.rs`. Testing
//! in-crate instead gives the same real-DB, real-`AdapterManager` coverage without
//! touching `lib.rs`.
//!
//! Split into two files (both under the 200-line guideline): `detector_tests.rs` for
//! the pure, DB-free detector functions (Step 3 — testable without the loop) and
//! `tick_tests.rs` for the real-DB, real-adapter `run_tick` behavior (Steps 4-5 —
//! cooldown + restart-survival proof).
mod detector_tests;
mod tick_tests;
