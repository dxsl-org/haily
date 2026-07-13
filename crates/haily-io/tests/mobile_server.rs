//! E2E + eval harness for the desktop mobile-server (Mobile Thin-Client plan phase 6).
//!
//! A plain `tokio-tungstenite` client drives the REAL `MobileAdapter`/axum app end to end —
//! pairing, auth, streaming, resume, epoch-restart, overflow-recovery, revocation, and
//! dead-approval reconcile — with zero network dependency beyond loopback and zero LLM. Only
//! the seams production code itself injects post-construction (device store, approval
//! resolver, session transcript, orchestrator sender) are fakes; see `mobile_server::support`.
//!
//! The whole file is gated on `mobile-server` so a default (no-feature) build never even tries
//! to compile against `haily_io::mobile`, which does not exist without the feature.
#![cfg(feature = "mobile-server")]

#[path = "mobile_server/auth_and_resume.rs"]
mod auth_and_resume;
#[path = "mobile_server/concurrency_and_approval.rs"]
mod concurrency_and_approval;
#[path = "mobile_server/pairing.rs"]
mod pairing;
#[path = "mobile_server/support.rs"]
mod support;
#[path = "mobile_server/wire_forward_compat_guard.rs"]
mod wire_forward_compat_guard;
