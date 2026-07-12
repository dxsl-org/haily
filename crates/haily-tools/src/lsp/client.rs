//! Long-lived language-server client: spawns a server as a persistent child, drives the
//! `async-lsp` JSON-RPC main loop over its stdio, and exposes the two capabilities the pipeline
//! needs — diagnostics (push-collected) and rename.
//!
//! # Isolation (P0 config applied, decision #3)
//! A language server executes project code during indexing (proc-macros, build scripts, plugins)
//! — the exact RCE surface the P0 sandbox exists for. The one-shot `Sandbox::exec` does NOT fit a
//! process that must stay alive across many stages, so this client manages the child + its pipes
//! itself, but APPLIES the P0 isolation CONFIG built by [`build_spawn_config`]: the
//! credential-scrubbed env allowlist ([`crate::exec::build_child_env`]), `NetworkPolicy::Off`
//! (servers index local code, never the network — the code-exec profile, not the browser one),
//! and the work-root as cwd. Full WSL2-sandboxed long-lived-server management (running the server
//! INSIDE the distro rather than only with its scrubbed env) is a runtime concern verified on a
//! host that actually has servers installed; the config seam here is what that will consume.

use super::registry::LspServerSpec;
use crate::exec::{build_child_env, NetworkPolicy};
use anyhow::{Context, Result};
use async_lsp::router::Router;
use async_lsp::{MainLoop, ServerSocket};
use lsp_types::notification::PublishDiagnostics;
use lsp_types::{Diagnostic, Url};
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

/// Diagnostics collected from the server's `textDocument/publishDiagnostics` pushes, keyed by the
/// document URI string. Shared between the main-loop router (writer) and the client (reader).
pub(crate) type DiagStore = Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>;

/// The fully-resolved, isolation-applied launch configuration for a language server. Built purely
/// by [`build_spawn_config`] (no spawning), so the isolation contract is unit-testable without a
/// server binary present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpawnConfig {
    pub program: String,
    pub args: Vec<String>,
    /// The child's COMPLETE environment (allowlist only — parent env is cleared, credentials
    /// scrubbed, `HOME`/`TMP` forced to the work root). See [`build_child_env`].
    pub envs: Vec<(String, String)>,
    /// Working directory = the workspace/work root the server indexes.
    pub cwd: PathBuf,
    /// Network posture — always [`NetworkPolicy::Off`] for a server (it indexes local code).
    pub network: NetworkPolicy,
}

/// Build the isolation-applied launch config for `spec` rooted at `work_root`. Env is the P0
/// credential-scrubbed allowlist with `HOME`/`TMP` pinned to the work root; network is forced
/// OFF; cwd is the work root. Pure — does not touch the process table.
pub fn build_spawn_config(spec: &LspServerSpec, work_root: &Path) -> ServerSpawnConfig {
    ServerSpawnConfig {
        program: spec.program.clone(),
        args: spec.args.clone(),
        // Scratch HOME = the work root: a server's cache/config lands inside the ephemeral
        // workspace, never the user's real home. No `extra` pairs — a server needs no injected
        // credentials (network is off; it only reads project code).
        envs: build_child_env(work_root, &[]),
        cwd: work_root.to_path_buf(),
        network: NetworkPolicy::Off,
    }
}

/// A live client bound to one running server process. Dropping it kills the child (kill-on-drop)
/// and aborts the main-loop task — the pool ([`super::LspManager`]) owns the client for the
/// workspace's lifetime and drops it on workspace close.
pub struct LspClient {
    socket: ServerSocket,
    diagnostics: DiagStore,
    // Kept alive for the client's lifetime; `kill_on_drop` tears the server down on close.
    _child: tokio::process::Child,
    loop_task: tokio::task::JoinHandle<()>,
}

impl LspClient {
    /// Spawn `config`'s server as a persistent child and start its JSON-RPC main loop.
    ///
    /// The child inherits ONLY `config.envs` (parent env cleared first — no credential leak) and
    /// runs with `config.cwd` as its working directory. Network isolation (`config.network`) is a
    /// property the sandboxed runtime enforces when servers are present; here the server is a
    /// local stdio child with no network arguments (documented adaptation, decision #3).
    ///
    /// # Errors
    /// Returns an error only if the child process cannot be spawned (e.g. the binary vanished
    /// between PATH discovery and launch) — an absent server is filtered EARLIER by the registry's
    /// availability check, so reaching here with a missing binary is the rare TOCTOU case.
    pub async fn spawn(config: ServerSpawnConfig) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(&config.program);
        cmd.args(&config.args)
            .current_dir(&config.cwd)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (k, v) in &config.envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().with_context(|| {
            format!("spawning language server '{}' (isolated)", config.program)
        })?;
        let stdout = child.stdout.take().context("server stdout pipe missing")?;
        let stdin = child.stdin.take().context("server stdin pipe missing")?;

        let diagnostics: DiagStore = Arc::new(Mutex::new(HashMap::new()));
        let (mainloop, socket) = new_client_loop(diagnostics.clone());
        // Drive the loop in the background: read the server's stdout, write to its stdin.
        // async-lsp speaks futures-io; adapt tokio's child pipes via tokio-util's compat shims.
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let loop_task = tokio::spawn(async move {
            if let Err(e) = mainloop.run_buffered(stdout.compat(), stdin.compat_write()).await {
                tracing::debug!("lsp main loop ended: {e}");
            }
        });

        Ok(Self { socket, diagnostics, _child: child, loop_task })
    }

    /// The server socket for issuing requests (initialize/rename/shutdown). Exposed to the tools
    /// module so it can drive capability requests without re-implementing the transport.
    pub(crate) fn socket(&self) -> ServerSocket {
        self.socket.clone()
    }

    /// Snapshot the diagnostics currently collected for `uri` (empty if none pushed yet).
    pub(crate) fn diagnostics_for(&self, uri: &Url) -> Vec<Diagnostic> {
        self.diagnostics
            .lock()
            .map(|g| g.get(uri.as_str()).cloned().unwrap_or_default())
            .unwrap_or_default()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // The child is killed by `kill_on_drop`; abort the loop task so it does not linger reading
        // a dead pipe.
        self.loop_task.abort();
    }
}

/// Construct the `async-lsp` client main loop + its server socket, wiring a router that collects
/// `publishDiagnostics` pushes into `diagnostics` and ignores every other server-initiated message
/// (a server sends `window/logMessage`, `$/progress`, `client/registerCapability`, … which a hint
/// layer does not act on — they must NOT break the loop). Factored out so both the live
/// [`LspClient::spawn`] path and the canned-frame protocol test share ONE transport wiring.
pub(crate) fn new_client_loop(
    diagnostics: DiagStore,
) -> (MainLoop<Router<DiagStore>>, ServerSocket) {
    MainLoop::new_client(|_server| {
        let mut router: Router<DiagStore> = Router::new(diagnostics);
        router.notification::<PublishDiagnostics>(|store, params| {
            if let Ok(mut g) = store.lock() {
                g.insert(params.uri.to_string(), params.diagnostics);
            }
            ControlFlow::Continue(())
        });
        // Ignore every other notification (logs/progress) — losing them is harmless for a hint
        // layer, and the default handler would BREAK the loop on an unknown method.
        router.unhandled_notification(|_, _| ControlFlow::Continue(()));
        // Answer any server->client request with null rather than method-not-found so a server
        // that probes (configuration/registerCapability) does not stall on startup.
        router.unhandled_request(|_, _req| async move { Ok(serde_json::Value::Null) });
        router.unhandled_event(|_, _| ControlFlow::Continue(()));
        router
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::NetworkPolicy;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn spawn_config_applies_p0_isolation() {
        // Simulate a parent holding a credential — it MUST NOT reach the server child.
        std::env::set_var("GH_TOKEN", "leak-me");
        let dir = tempfile::tempdir().unwrap();
        let spec = LspServerSpec { language: "rust", program: "rust-analyzer".into(), args: vec![] };
        let cfg = build_spawn_config(&spec, dir.path());
        std::env::remove_var("GH_TOKEN");

        // Network OFF (server indexes local code, never the browser network-on profile).
        assert_eq!(cfg.network, NetworkPolicy::Off);
        // cwd = the work root the server indexes.
        assert_eq!(cfg.cwd, dir.path());
        let keys: Vec<&str> = cfg.envs.iter().map(|(k, _)| k.as_str()).collect();
        // Credential scrubbed (P0 allowlist, not a denylist).
        assert!(!keys.contains(&"GH_TOKEN"), "credential leaked into server env: {keys:?}");
        // HOME forced to the ephemeral work root (server cache never lands in the real home).
        let home = cfg.envs.iter().find(|(k, _)| k == "HOME").map(|(_, v)| v.as_str());
        assert_eq!(home, Some(dir.path().to_string_lossy().as_ref()));
    }

    /// The protocol/framing test WITHOUT a live server: drive the client main loop over an
    /// in-memory duplex, write a canned `publishDiagnostics` LSP frame as if a server sent it, and
    /// assert the client parses + collects it. Proves the transport + diagnostics parsing without
    /// any language server installed (the deferred live smoke covers real servers).
    #[tokio::test]
    async fn parses_diagnostics_from_a_canned_server_frame() {
        let diagnostics: DiagStore = Arc::new(Mutex::new(HashMap::new()));
        let (mainloop, _socket) = new_client_loop(diagnostics.clone());

        // `client_end` is the client's view; the test acts as the SERVER on `server_end`.
        let (client_end, mut server_end) = tokio::io::duplex(8192);
        let (client_read, client_write) = tokio::io::split(client_end);
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let loop_task = tokio::spawn(async move {
            let _ = mainloop.run_buffered(client_read.compat(), client_write.compat_write()).await;
        });

        // A canned publishDiagnostics notification (one error) framed with Content-Length.
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///proj/src/main.rs",
                "diagnostics": [{
                    "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}},
                    "severity": 1,
                    "message": "mismatched types: expected u32, found String"
                }]
            }
        })
        .to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        server_end.write_all(frame.as_bytes()).await.unwrap();
        server_end.flush().await.unwrap();

        // Poll until the router has processed the frame (bounded so a regression fails fast).
        let uri = Url::parse("file:///proj/src/main.rs").unwrap();
        let mut got = Vec::new();
        for _ in 0..50 {
            got = diagnostics
                .lock()
                .map(|g| g.get(uri.as_str()).cloned().unwrap_or_default())
                .unwrap_or_default();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        loop_task.abort();

        assert_eq!(got.len(), 1, "the canned diagnostic must be parsed + collected");
        assert!(got[0].message.contains("mismatched types"));
    }
}
