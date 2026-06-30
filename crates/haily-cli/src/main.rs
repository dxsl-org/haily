mod runtime;

use anyhow::Result;
use clap::{Parser, Subcommand};
use haily_io::AdapterManager;
use std::{path::PathBuf, sync::Arc};
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Cli::parse();
    let data_dir = args.data_dir.unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;

    match args.mode.unwrap_or(Mode::Cli) {
        Mode::Cli => run_cli(data_dir).await,
        Mode::Headless => run_headless(data_dir).await,
        Mode::Gui => run_gui(),
    }
}

fn default_data_dir() -> PathBuf {
    // Portable-first: store data next to the exe in ./data/
    // Falls back to relative "data/" if current_exe() fails (e.g. unit tests).
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("data")))
        .unwrap_or_else(|| PathBuf::from("data"))
}

async fn run_cli(data_dir: PathBuf) -> Result<()> {
    let (_, _, orc) = runtime::init(&data_dir).await?;

    eprintln!("Haily — CLI  |  LLM: {}  |  data: {}", orc.llm_provider(), data_dir.display());
    eprintln!("Type a message and press Enter. Ctrl+D to quit.\n");

    let am = AdapterManager::builder()
        .register(Arc::new(haily_io::CliAdapter::new()))
        .build();

    runtime::dispatch_loop(am, orc).await
}

async fn run_headless(data_dir: PathBuf) -> Result<()> {
    let (db, _kms, orc) = runtime::init(&data_dir).await?;

    #[allow(unused_mut)]
    let mut builder = AdapterManager::builder();

    #[cfg(feature = "telegram")]
    match std::env::var("TELEGRAM_BOT_TOKEN") {
        Ok(token) => {
            builder = builder.register(Arc::new(haily_io::TelegramAdapter::new(Some(token))));
        }
        Err(_) => {
            warn!("TELEGRAM_BOT_TOKEN not set — Telegram adapter disabled");
        }
    }

    #[cfg(not(feature = "telegram"))]
    warn!(
        "headless mode: telegram feature is not compiled in. \
         Rebuild with `--features telegram` to enable the Telegram adapter."
    );

    eprintln!("Haily — Headless  |  LLM: {}  |  data: {}", orc.llm_provider(), data_dir.display());

    let am = builder.build();

    haily_proactive::ProactiveDaemon::new(db, am.clone()).start();
    runtime::dispatch_loop(am, orc).await
}

fn run_gui() -> Result<()> {
    // Phase 10: haily_tauri::run() will be called here.
    anyhow::bail!(
        "GUI mode is implemented in Phase 10 (Tauri). \
         Run `haily cli` for the terminal interface, or `haily headless` for daemon mode."
    )
}
