use anyhow::Result;
use clap::{Parser, Subcommand};
use haily_app::{AppHandle, BootstrapOptions};
use haily_io::{AcpAdapter, Adapter, CliAdapter};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Parser)]
#[command(name = "haily", version, about = "Haily — personal AI assistant")]
struct Cli {
    #[command(subcommand)]
    mode: Option<Mode>,

    /// Override the data directory (default: OS user-data dir / haily)
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Mode {
    /// Interactive terminal REPL
    Cli,
    /// Background daemon — Telegram bot + proactive engine (requires TELEGRAM_BOT_TOKEN)
    Headless,
    /// Desktop GUI (Tauri) — use `haily gui` or run without a subcommand on desktop
    Gui,
    /// ACP (Agent Client Protocol) coding channel over stdio (Phase 12). Speaks
    /// newline-delimited JSON-RPC 2.0 on stdin/stdout so an ACP-capable editor (Zed and
    /// friends) becomes a code-viewing/reviewing front-end for Haily's coding pipeline.
    /// stdout is RESERVED for protocol frames — all logs go to stderr.
    Acp,
    /// Write a consistent standalone copy of the database to the given path (Phase 6,
    /// manual export — same `VACUUM INTO` mechanism the scheduled backup worker uses).
    Export {
        /// Destination file path. The parent directory must already exist; an existing
        /// file at this path is overwritten.
        path: PathBuf,
    },
    /// Golden coding eval (Phase 9). CLI-only — the eval-mode plan-gate bypass is never
    /// reachable from a chat request (SEC-H).
    Eval {
        #[command(subcommand)]
        kind: EvalKind,
    },
    /// Pair a mobile device (Mobile Thin-Client plan phase 2a) — headless fallback, since
    /// there is no GUI here to render the QR (that's the future P2b "Add Device" screen).
    /// Mints a short-TTL pairing code, prints it as text + ASCII QR, and runs a one-shot
    /// mobile server (tailnet + loopback only, per the M2 bind policy) until Ctrl+C.
    #[cfg(feature = "mobile-server")]
    Pair,
}

#[derive(Subcommand)]
enum EvalKind {
    /// Run the coding fixtures under `evals/fixtures/` and score them by their own gates.
    /// Requires `HAILY_EVAL_MODEL` + a configured LLM router; prints guidance and exits
    /// cleanly when no model host is configured (the baseline matrix is a manual step).
    Coding {
        /// Judgment depth: quick | normal | deep (default normal).
        #[arg(long, default_value = "normal")]
        depth: String,
        /// Enable P3 tier escalation on gate failure (the `{off,on}` matrix arm).
        #[arg(long, default_value_t = false)]
        escalate: bool,
        /// Override the fixtures directory (default `evals/fixtures`).
        #[arg(long)]
        fixtures: Option<PathBuf>,
    },
}

/// Windows console-close (`CTRL_CLOSE_EVENT`) gives a hard ~5s OS kill window before
/// a force-kill — budget the drain well under that. Unix has no equivalent hard
/// deadline; the same constant is reused there for consistency, not necessity.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to STDERR on every mode. This is load-bearing for the ACP channel (Phase 12):
    // its stdout is reserved for JSON-RPC frames, so a stray log on stdout would corrupt the
    // stream. Harmless for the other modes — the CLI REPL writes chat via direct stdout writes,
    // not tracing, and GUI/headless never depended on logs landing on stdout.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args = Cli::parse();
    let data_dir = args.data_dir.unwrap_or_else(haily_app::default_data_dir);
    std::fs::create_dir_all(&data_dir)?;

    match args.mode.unwrap_or(Mode::Cli) {
        Mode::Cli => run_cli(data_dir).await,
        Mode::Headless => run_headless(data_dir).await,
        Mode::Gui => run_gui(),
        Mode::Acp => run_acp(data_dir).await,
        Mode::Export { path } => run_export(data_dir, path).await,
        Mode::Eval { kind } => run_eval(data_dir, kind).await,
        #[cfg(feature = "mobile-server")]
        Mode::Pair => run_pair(data_dir).await,
    }
}

/// `haily eval coding …` — the CLI-only coding eval entry (SEC-H: eval mode is minted from a
/// CLI-origin request inside `haily_app::run_coding_eval_all`, never from a chat request).
async fn run_eval(data_dir: PathBuf, kind: EvalKind) -> Result<()> {
    match kind {
        EvalKind::Coding {
            depth,
            escalate,
            fixtures,
        } => haily_app::run_coding_eval_all(&data_dir, &depth, escalate, fixtures).await,
    }
}

/// `haily export <path>` — writes a consistent standalone copy of the database (same
/// `VACUUM INTO` mechanism the scheduled backup worker uses) without starting the full
/// app. The exported file is unencrypted and contains all local data, same trust
/// boundary as `haily.db` itself — warned about here since there is no GUI dialog to
/// carry that copy in this mode.
async fn run_export(data_dir: PathBuf, dest: PathBuf) -> Result<()> {
    eprintln!("Warning: the exported file is unencrypted and contains all local data.");
    haily_app::export_database(&data_dir, &dest).await?;
    eprintln!("Database exported to {}", dest.display());
    Ok(())
}

async fn run_cli(data_dir: PathBuf) -> Result<()> {
    let cli = Arc::new(CliAdapter::new());
    let eof = cli.eof_token();
    let adapters: Vec<Arc<dyn Adapter>> = vec![cli];
    let handle = AppHandle::bootstrap(&data_dir, adapters, BootstrapOptions::default()).await?;

    eprintln!(
        "Haily — CLI  |  LLM: {}  |  data: {}",
        handle.orchestrator.llm_provider(),
        data_dir.display()
    );
    eprintln!("Type a message and press Enter. Ctrl+D to quit.\n");

    wait_for_shutdown_signal(eof).await;
    handle.shutdown(SHUTDOWN_TIMEOUT).await;
    Ok(())
}

/// `haily acp` — the ACP coding channel over stdio (Phase 12). Wires a single [`AcpAdapter`]
/// into the standard bootstrap; it plugs into the existing adapter vec, so the approval
/// resolver, kill switch, and session-transcript provider are injected automatically.
///
/// stdout is RESERVED for JSON-RPC frames — every human-facing line here goes to stderr. The
/// process shuts down when the editor closes stdin (the adapter's EOF token) or on an OS signal.
async fn run_acp(data_dir: PathBuf) -> Result<()> {
    let acp = Arc::new(AcpAdapter::new());
    let eof = acp.eof_token();
    let adapters: Vec<Arc<dyn Adapter>> = vec![acp];
    let handle = AppHandle::bootstrap(&data_dir, adapters, BootstrapOptions::default()).await?;

    eprintln!(
        "Haily — ACP  |  LLM: {}  |  data: {}",
        handle.orchestrator.llm_provider(),
        data_dir.display()
    );
    eprintln!("ACP stdio server ready. Point an ACP-capable editor (e.g. Zed) at this process.");

    wait_for_shutdown_signal(eof).await;
    handle.shutdown(SHUTDOWN_TIMEOUT).await;
    Ok(())
}

async fn run_headless(data_dir: PathBuf) -> Result<()> {
    #[allow(unused_mut)]
    let mut adapters: Vec<Arc<dyn Adapter>> = Vec::new();

    #[cfg(feature = "telegram")]
    match std::env::var("TELEGRAM_BOT_TOKEN") {
        Ok(token) => adapters.push(Arc::new(haily_io::TelegramAdapter::new(Some(token)))),
        Err(_) => warn!("TELEGRAM_BOT_TOKEN not set — Telegram adapter disabled"),
    }

    #[cfg(not(feature = "telegram"))]
    warn!(
        "headless mode: telegram feature is not compiled in. \
         Rebuild with `--features telegram` to enable the Telegram adapter."
    );

    // Mobile Thin-Client plan phase 2a: a SEPARATE `DbHandle` from the one `AppHandle::bootstrap`
    // opens below — the mobile adapter must be fully constructed (with its config + device
    // store) BEFORE the adapters `Vec` is handed to `bootstrap`, which is the point the real
    // orchestrator/KMS DB connection is created. Opening a second pool onto the same
    // `haily.db` file is the same pattern `export_database` already uses for one-shot work;
    // WAL mode supports concurrent connections from multiple pools safely.
    #[cfg(feature = "mobile-server")]
    {
        let mobile_db = Arc::new(haily_db::DbHandle::init(&data_dir.join("haily.db")).await?);
        let mobile_cfg = haily_app::mobile_config::load_mobile_config(&mobile_db).await;
        if mobile_cfg.enabled {
            let device_store = Arc::new(haily_app::mobile_device_store::DbMobileDeviceStore::new(
                Arc::clone(&mobile_db),
            ));
            adapters.push(Arc::new(haily_io::mobile::MobileAdapter::new(
                mobile_cfg,
                device_store,
                data_dir.clone(),
            )));
        } else {
            tracing::info!("mobile: server disabled (set the 'mobile.enabled' preference to enable) — not starting");
        }
    }

    // M5a: Session-0/no-D-Bus headless daemons cannot reliably reach the OS keyring
    // (DPAPI needs the interactive session; Linux secret-service needs a D-Bus session
    // bus) — never attempt it here, go straight to the DB-read path with a persisted
    // fallback warning instead of hanging or erroring on every credential read.
    let opts = BootstrapOptions {
        attempt_keyring: false,
        ..BootstrapOptions::default()
    };
    let handle = AppHandle::bootstrap(&data_dir, adapters, opts).await?;

    // M6b (Activate-and-Measure phase 4b): gate visibility, not activation — the env/file
    // credential source (`HAILY_CRED_FILE` / `HAILY_CRED__*`) can be provisioned later
    // without a restart (see `CredentialStore::headless_env_source`), so hard-blocking
    // connector registration here would be wrong. This is a soft gate: warn loudly so an
    // operator is never left assuming headless connectors work when every auth-requiring
    // call will actually fail closed one at a time.
    if haily_app::load_odoo_api_key(&handle.credential_store)
        .await
        .is_none()
    {
        warn!(
            "headless: no Odoo connector credential resolvable (HAILY_CRED_FILE, \
             HAILY_CRED__CONNECTOR_ODOO_API_KEY, or HAILY_ODOO_API_KEY) — any connector call \
             requiring auth will fail closed until one is configured"
        );
    }

    eprintln!(
        "Haily — Headless  |  LLM: {}  |  data: {}",
        handle.orchestrator.llm_provider(),
        data_dir.display()
    );

    // Headless has no interactive stdin; a never-cancelled token means "OS signals only".
    wait_for_shutdown_signal(CancellationToken::new()).await;
    handle.shutdown(SHUTDOWN_TIMEOUT).await;
    Ok(())
}

/// `haily pair` — headless pairing ceremony (Mobile Thin-Client plan phase 2a). There is no
/// GUI here to render the future "Add Device" dialog (P2b), so this command itself IS the
/// out-of-band confirm (M4): an operator with terminal access to the trusted desktop
/// explicitly ran it, which is at least as strong a proof of physical access as tapping
/// "Approve" on a dialog — see `haily-io::mobile::pairing`'s module doc for the full rationale.
/// Runs a one-shot mobile server (same bind policy as `haily headless`'s, tailnet + loopback
/// only unless `mobile.lan_opt_in` is set) until Ctrl+C.
#[cfg(feature = "mobile-server")]
async fn run_pair(data_dir: PathBuf) -> Result<()> {
    let db = Arc::new(haily_db::DbHandle::init(&data_dir.join("haily.db")).await?);
    let device_store = Arc::new(haily_app::mobile_device_store::DbMobileDeviceStore::new(
        Arc::clone(&db),
    ));
    let mut cfg = haily_app::mobile_config::load_mobile_config(&db).await;
    cfg.enabled = true; // force on for this one-shot ceremony even if the persisted pref is off
    let port = cfg.port;

    let adapter = haily_io::mobile::MobileAdapter::new(cfg, device_store, data_dir.clone());
    let code = adapter.mint_pairing_code(None, true);

    let cert = haily_io::mobile::tls::load_or_generate(&data_dir)?;
    let interfaces = haily_io::mobile::bind::enumerate_interfaces();
    let host = haily_io::mobile::bind::select_bind_addrs(&interfaces, false, port)
        .into_iter()
        .find(|a| !a.ip().is_loopback())
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let expires_at = (chrono::Utc::now()
        + chrono::Duration::from_std(haily_io::mobile::pairing::PAIRING_CODE_TTL)?)
    .to_rfc3339();
    let qr = haily_types::PairingQr {
        host,
        port,
        cert_fingerprint: cert.fingerprint,
        pairing_code: code.clone(),
        expires_at,
    };

    // Review finding 6c: bind BEFORE printing anything the phone would otherwise be told to
    // scan against a server that never actually came up — most commonly because `haily
    // headless`/GUI is already running the mobile server on this same port.
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    if !adapter.start_and_await_bind(tx).await {
        anyhow::bail!(
            "mobile: failed to bind any address on port {port} — is `haily headless` or the \
             GUI already running the mobile server? Stop it first, or set a different \
             'mobile.port' preference for this ceremony."
        );
    }

    eprintln!("Pairing code: {code}  (valid 2 minutes)");
    eprintln!("Payload: {}", serde_json::to_string(&qr)?);
    if let Ok(ascii_qr) = qrcode::QrCode::new(serde_json::to_string(&qr)?) {
        eprintln!(
            "{}",
            ascii_qr
                .render::<qrcode::render::unicode::Dense1x2>()
                .build()
        );
    }
    eprintln!("Waiting for the phone to scan and pair — press Ctrl+C when done.");

    tokio::signal::ctrl_c().await.ok();
    Ok(())
}

fn run_gui() -> Result<()> {
    // Phase 10: haily_tauri::run() will be called here.
    anyhow::bail!(
        "GUI mode is implemented in Phase 10 (Tauri). \
         Run `haily cli` for the terminal interface, or `haily headless` for daemon mode."
    )
}

/// Races every OS-delivered "please stop" signal and returns on the first one.
///
/// `tokio::signal::ctrl_c()` alone only catches `CTRL_C_EVENT` on Windows — console
/// window close, logoff, and system shutdown are distinct signals
/// (`CTRL_CLOSE`/`CTRL_LOGOFF`/`CTRL_SHUTDOWN`) that must be registered separately.
/// On non-Windows, `SIGTERM` is the equivalent "please stop" signal Ctrl+C alone
/// would miss (e.g. from a process manager or `kill`).
async fn wait_for_shutdown_signal(eof: CancellationToken) {
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
        // Registering console control handlers can fail when no console is attached
        // (e.g. launched under a Windows service manager). Degrade to Ctrl+C + EOF
        // rather than aborting the process.
        match (ctrl_close(), ctrl_logoff(), ctrl_shutdown()) {
            (Ok(mut close), Ok(mut logoff), Ok(mut shutdown)) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = close.recv() => {}
                    _ = logoff.recv() => {}
                    _ = shutdown.recv() => {}
                    _ = eof.cancelled() => {}
                }
            }
            _ => {
                warn!("could not register console control handlers — using Ctrl+C + EOF only");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = eof.cancelled() => {}
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                    _ = eof.cancelled() => {}
                }
            }
            Err(e) => {
                warn!("could not register SIGTERM handler ({e}) — using Ctrl+C + EOF only");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = eof.cancelled() => {}
                }
            }
        }
    }
}
