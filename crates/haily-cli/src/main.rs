use anyhow::Result;
use clap::{Parser, Subcommand};
use haily_app::{AppHandle, BootstrapOptions};
use haily_io::{Adapter, CliAdapter};
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
}

/// Windows console-close (`CTRL_CLOSE_EVENT`) gives a hard ~5s OS kill window before
/// a force-kill — budget the drain well under that. Unix has no equivalent hard
/// deadline; the same constant is reused there for consistency, not necessity.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Cli::parse();
    let data_dir = args.data_dir.unwrap_or_else(haily_app::default_data_dir);
    std::fs::create_dir_all(&data_dir)?;

    match args.mode.unwrap_or(Mode::Cli) {
        Mode::Cli => run_cli(data_dir).await,
        Mode::Headless => run_headless(data_dir).await,
        Mode::Gui => run_gui(),
    }
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

    // M5a: Session-0/no-D-Bus headless daemons cannot reliably reach the OS keyring
    // (DPAPI needs the interactive session; Linux secret-service needs a D-Bus session
    // bus) — never attempt it here, go straight to the DB-read path with a persisted
    // fallback warning instead of hanging or erroring on every credential read.
    let opts = BootstrapOptions {
        attempt_keyring: false,
        ..BootstrapOptions::default()
    };
    let handle = AppHandle::bootstrap(&data_dir, adapters, opts).await?;

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
