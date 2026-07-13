//! Mobile Thin-Client plan phase 3 — the host-compilable half of the mobile app's Rust core.
//!
//! This crate is the ONLY mobile-facing crate the main workspace/CI compiles (C2): it depends
//! only on `haily-types` + a WS/TLS/serde stack, with no Android/iOS-only crates, so
//! `cargo test -p haily-mobile-client` runs on any host. The actual Tauri shell
//! (`src-tauri-mobile/`) lives in its OWN, separate Cargo workspace and consumes this crate as
//! a path dependency — see `docs/mobile-protocol.md` and the phase-03 plan file for why the
//! split exists.
//!
//! Module map:
//! - [`cert_verify`] — the GATING cert-pin spike: a `rustls::ClientConfig` that trusts exactly
//!   one pinned SHA-256 fingerprint (mirrors `haily-io::mobile::tls`'s server-side identity).
//! - [`codec`] — envelope encode/decode; `ServerBody::Unknown`/`ClientFrame::Unknown` already
//!   degrade gracefully in `haily-types` (C3) — this module is the thin wire-level wrapper.
//! - [`endpoints`] — MagicDNS resolve-check → mDNS → QR-literal-host endpoint selection order.
//! - [`reconnect`] — resume cursor (epoch/seq), dedup, and exponential backoff — pure, unit
//!   tested logic with no network I/O.
//! - [`ws`] — the actual TLS+WebSocket connect primitive (token header, Hello/HelloAck).
//! - [`client`] — the driving loop composing the above into "stay connected, forward frames,
//!   reconnect on drop" — `src-tauri-mobile`'s command layer is the only consumer.
//! - [`tts_chunker`] — sentence-boundary chunker for streaming TTS (phase 4): pure text
//!   processing, no platform dependency, shared by Android (now) and iOS (P5).

pub mod cert_verify;
pub mod client;
pub mod codec;
pub mod endpoints;
pub mod reconnect;
pub mod tts_chunker;
pub mod ws;

pub use cert_verify::pinned_client_config;
pub use client::{spawn, ClientEvent, ClientHandle, MobileClientConfig, StopReason};
pub use reconnect::{Backoff, ResumeCursor, SeqDedup};
pub use tts_chunker::TtsChunker;
pub use ws::ConnectError;
