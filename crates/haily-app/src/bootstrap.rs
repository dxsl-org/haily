//! Shared startup sequence: DB → KMS → Orchestrator → adapters → dispatch/watcher/daemon.
//!
//! One `AppHandle` is built per process and owned by the mode-specific entry point
//! (CLI `main.rs`, Tauri `lib.rs`). It is the single shutdown surface: dropping it
//! without calling `shutdown()` leaves background tasks running until the process
//! exits — always call `shutdown()` on the signal/exit-event path.
use crate::auto_approve::{load_auto_approve, validate_auto_approve};
use crate::config::load_llm_config;
use crate::turns::TurnRegistry;
use crate::{dispatch, watchers};
use anyhow::Result;
use haily_core::Orchestrator;
use haily_db::DbHandle;
use haily_io::{Adapter, AdapterManager};
use haily_kms::KmsHandle;
use haily_tools::ToolRegistry;
use std::{path::Path, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};
use uuid::Uuid;

/// Toggles for subsystems that historically differed between modes (F6 mode
/// asymmetry). Both default to `true` — every mode gets the full feature set unless
/// a caller has a specific reason to opt out (e.g. a future test harness).
#[derive(Debug, Clone, Copy)]
pub struct BootstrapOptions {
    pub enable_daemon: bool,
    pub enable_watcher: bool,
}

impl Default for BootstrapOptions {
    fn default() -> Self {
        Self {
            enable_daemon: true,
            enable_watcher: true,
        }
    }
}

/// Owns every long-lived handle for one running instance of the app.
///
/// `shutdown` is the root `CancellationToken` — every subsystem holds a `child_token()`
/// derived from it, so cancelling it here cancels all of them atomically. `tasks` is
/// the root `TaskTracker` — every spawned subsystem task (dispatch loop, work-item
/// watcher, proactive daemon loops, self-improvement workers, and each per-turn
/// request task) is registered on it, so `shutdown()` can prove they have actually
/// exited rather than just requesting that they do.
pub struct AppHandle {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub orchestrator: Arc<Orchestrator>,
    pub adapters: AdapterManager,
    shutdown: CancellationToken,
    tasks: TaskTracker,
    turns: Arc<TurnRegistry>,
}

impl AppHandle {
    /// Initialise the full stack and start the dispatch loop, work-item watcher, and
    /// proactive daemon (per `opts`). Returns once every subsystem has been spawned —
    /// none of this blocks waiting for requests.
    pub async fn bootstrap(
        data_dir: &Path,
        adapters: Vec<Arc<dyn Adapter>>,
        opts: BootstrapOptions,
    ) -> Result<Self> {
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        let turns = Arc::new(TurnRegistry::new());

        let db_path = data_dir.join("haily.db");
        info!("DB: {}", db_path.display());
        let db = DbHandle::init(&db_path).await?;

        info!("KMS: loading HNSW index (dump if present, else rebuild from DB)…");
        let kms = KmsHandle::init(db.clone(), data_dir).await?;

        let db = Arc::new(db);
        let kms = Arc::new(kms);
        let llm_cfg = load_llm_config(&kms).await;

        // Validate the auto_approve allowlist against the same tool set the
        // orchestrator will build from. A destructive/exfil tool listed here is a
        // config error at boot — never silently ignored, never auto-approved.
        let auto_approve_raw = load_auto_approve(&kms).await;
        let auto_approve = validate_auto_approve(&auto_approve_raw, &ToolRegistry::build_v1())?;

        let orchestrator = Arc::new(
            Orchestrator::init(
                Arc::clone(&kms),
                Arc::clone(&db),
                llm_cfg,
                shutdown.child_token(),
                tasks.clone(),
                auto_approve,
            )
            .await?,
        );

        info!(llm = orchestrator.llm_provider(), "ready");

        // Adapters are constructed by the caller before the orchestrator (and its
        // approval broker) exist, so the resolver is injected here — after `init`,
        // before `start_all` begins accepting requests — rather than at construction.
        let resolver = orchestrator.approval_resolver();
        let kill = orchestrator.kill_handle();
        for adapter in &adapters {
            adapter.set_approval_resolver(Arc::clone(&resolver));
            adapter.set_kill_switch(Arc::clone(&kill));
        }

        let mut builder = AdapterManager::builder();
        for adapter in adapters {
            builder = builder.register(adapter);
        }
        let am = builder.build();

        dispatch::spawn_dispatch_loop(
            am.clone(),
            Arc::clone(&orchestrator),
            shutdown.child_token(),
            tasks.clone(),
            Arc::clone(&turns),
        )
        .await?;

        if opts.enable_watcher {
            watchers::spawn_work_item_watcher(
                Arc::clone(&db),
                am.clone(),
                shutdown.child_token(),
                tasks.clone(),
            );
        }

        if opts.enable_daemon {
            watchers::spawn_proactive_daemon(
                Arc::clone(&db),
                am.clone(),
                shutdown.child_token(),
                tasks.clone(),
            );
        }

        // Phase 3: periodically purge action-journal rows past their retention window so
        // recorded PII (request_params/pre_state/post_state) is bounded. Registered on the
        // root tracker + selecting on shutdown, same contract as the watcher/daemon.
        watchers::spawn_journal_purge(Arc::clone(&db), shutdown.child_token(), tasks.clone());

        info!(
            watcher = opts.enable_watcher,
            daemon = opts.enable_daemon,
            "startup complete — dispatch loop running"
        );

        Ok(Self {
            db,
            kms,
            orchestrator,
            adapters: am,
            shutdown,
            tasks,
            turns,
        })
    }

    /// Cancel the in-flight turn for `session_id`, if any. Delegates to the shared
    /// `TurnRegistry` — see `turns::TurnRegistry::cancel` for the exact semantics.
    /// Returns `false` (not an error) when `session_id` has no registered turn
    /// (already finished, unknown, or never started); callers should treat that as
    /// "nothing to do", mirroring `approve_tool`'s convention for stale ids.
    pub fn cancel_turn(&self, session_id: Uuid) -> bool {
        self.turns.cancel(session_id)
    }

    /// Shared handle to the turn registry, for callers (e.g. the Tauri command layer)
    /// that want to hold their own `Arc` clone rather than locking `app` per call —
    /// mirrors `Orchestrator::approval_resolver()`'s "clone the handle once at setup"
    /// pattern.
    pub fn turn_registry(&self) -> Arc<TurnRegistry> {
        Arc::clone(&self.turns)
    }

    /// Number of tasks currently registered on the root `TaskTracker` — dispatch loop,
    /// watcher, daemon loops, self-improvement workers, plus one per in-flight turn.
    /// Exposed for startup diagnostics and tests; not meaningful as a health signal on
    /// its own (a healthy idle app still has a nonzero, mode-dependent count).
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    /// Ordered graceful shutdown: stop intake → drain in-flight work → flush.
    ///
    /// 1. Cancel the root token — every `select!` arm across dispatch/watcher/daemon/
    ///    worker loops observes this and stops accepting new work.
    /// 2. Close the tracker (no new tasks may register) and wait for every tracked
    ///    task to finish, bounded by `timeout`.
    /// 3. Dump the HNSW index (phase-08) so next startup can load instead of
    ///    rebuilding from DB. Best-effort — a failure here only costs the next
    ///    startup a rebuild, never data, so it must not block or fail shutdown.
    /// 4. Best-effort SQLite WAL checkpoint so no large `-wal` file lingers.
    ///
    /// Scope note (see phase risk notes): a llama.cpp generation in flight is a
    /// synchronous `spawn_blocking` token loop with no cancellation check — it is not
    /// preemptible by this call and is abandoned; its `WorkItem` is left `running` for
    /// the existing boot-time `reset_stale_running` crash-recovery path to reclaim on
    /// next start. The drain guarantee here covers cloud turns, workers, and watchers.
    pub async fn shutdown(self, timeout: Duration) {
        info!("shutdown: stopping intake");
        self.shutdown.cancel();
        self.tasks.close();

        match tokio::time::timeout(timeout, self.tasks.wait()).await {
            Ok(()) => info!("shutdown: all tasks drained"),
            Err(_) => warn!(
                timeout_secs = timeout.as_secs(),
                "shutdown: timed out waiting for tasks — proceeding with exit"
            ),
        }

        self.kms.flush_index().await;

        match self.db.wal_checkpoint_truncate().await {
            Ok(false) => info!("shutdown: WAL checkpoint complete"),
            Ok(true) => warn!(
                "shutdown: WAL checkpoint was busy (a connection still held a lock, likely an \
                 abandoned turn) — WAL left un-truncated but crash-safe"
            ),
            Err(e) => warn!("shutdown: WAL checkpoint failed: {e:#}"),
        }
    }
}
