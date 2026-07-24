//! Shared startup sequence: DB → KMS → Orchestrator → adapters → dispatch/watcher/daemon.
//!
//! One `AppHandle` is built per process and owned by the mode-specific entry point
//! (CLI `main.rs`, Tauri `lib.rs`). It is the single shutdown surface: dropping it
//! without calling `shutdown()` leaves background tasks running until the process
//! exits — always call `shutdown()` on the signal/exit-event path.
use crate::auto_approve::{load_auto_approve, validate_auto_approve};
use crate::config::{load_llm_config, ODOO_API_KEY_PREF};
use crate::credential_store::{is_keyring_marker, CredentialPolicy, CredentialStore};
use crate::notify::{NoopNotifier, OsNotifier, ToastCoalescer};
use crate::run_control::RunControlRegistry;
use crate::slash_registry::SlashRegistry;
use crate::turns::TurnRegistry;
use crate::{dispatch, reaper, watchers};
use anyhow::Result;
use haily_core::Orchestrator;
use haily_db::{queries::meta, DbHandle};
use haily_io::{Adapter, AdapterManager};
use haily_kms::KmsHandle;
use haily_tools::ToolRegistry;
use std::{path::Path, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};
use uuid::Uuid;

/// Toggles for subsystems that historically differed between modes (F6 mode
/// asymmetry). `enable_daemon`/`enable_watcher` default to `true` — every mode gets the
/// full feature set unless a caller has a specific reason to opt out (e.g. a future test
/// harness). `attempt_keyring` defaults to `true` (interactive desktop/CLI) and MUST be
/// set `false` by the `--headless` launch path (M5a) — Windows Credential Manager (DPAPI,
/// tied to the interactive session) and Linux secret-service (needs a D-Bus session bus)
/// are both structurally unavailable in a true daemon/Session-0 context, so a headless
/// boot that still tried the keyring could hang or error on every credential read.
///
/// `os_notifier` (Unified Chat UI phase 7, D7) is the seam for a real OS toast — the Tauri shell
/// constructs its window-focus-aware implementation BEFORE calling `bootstrap` (a Tauri
/// `AppHandle` already exists at that point in `src-tauri`'s `setup()`) and overrides this field;
/// every other caller (CLI, headless, every test in this crate) keeps the `Default`
/// [`NoopNotifier`]. Not `Copy` (an `Arc<dyn OsNotifier>` isn't) — no caller relies on `Copy`
/// semantics for this struct (every call site constructs or `..Default::default()`s it fresh).
#[derive(Clone)]
pub struct BootstrapOptions {
    pub enable_daemon: bool,
    pub enable_watcher: bool,
    pub attempt_keyring: bool,
    pub os_notifier: Arc<dyn OsNotifier>,
}

impl std::fmt::Debug for BootstrapOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapOptions")
            .field("enable_daemon", &self.enable_daemon)
            .field("enable_watcher", &self.enable_watcher)
            .field("attempt_keyring", &self.attempt_keyring)
            .field("os_notifier", &"<dyn OsNotifier>")
            .finish()
    }
}

impl Default for BootstrapOptions {
    fn default() -> Self {
        Self {
            enable_daemon: true,
            enable_watcher: true,
            attempt_keyring: true,
            os_notifier: Arc::new(NoopNotifier),
        }
    }
}

/// Write a consistent standalone copy of the database at `data_dir` to `dest_path`
/// (Phase 6 manual export — backs both the CLI `export` subcommand and the GUI export
/// command). Opens the DB directly rather than running the full [`AppHandle::bootstrap`]
/// (LLM router, orchestrator, adapters, background workers): none of that is needed for
/// a one-shot file copy, and starting it would have real side effects (e.g. spawning the
/// proactive daemon) for a command that should just write a file and exit.
///
/// # Errors
/// Returns an error if the source DB cannot be opened or [`DbHandle::backup_to`] fails
/// (e.g. `dest_path`'s parent directory does not exist).
pub async fn export_database(data_dir: &Path, dest_path: &Path) -> Result<()> {
    let db = DbHandle::init(&data_dir.join("haily.db")).await?;
    db.backup_to(dest_path).await
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
    /// OS-keyring-backed credential store (Harness Completion phase 4). Exposed so a mode
    /// layer (e.g. the Tauri command surface) can read/rotate a connector secret through
    /// the same read/write-fallback policy the startup migration used — never by reaching
    /// into `kms_preferences` directly for a credential.
    pub credential_store: Arc<CredentialStore>,
    /// `pub(crate)` (not private): `launch.rs`'s coding-run entrypoint needs its own child
    /// token per launch, mirroring `dispatch_loop`'s own `shutdown.child_token()` per turn.
    pub(crate) shutdown: CancellationToken,
    /// `pub(crate)`: `launch.rs` registers the run-event/distillation bridges plus the tracked
    /// launch task on this SAME tracker, so `AppHandle::shutdown`'s drain covers them too.
    pub(crate) tasks: TaskTracker,
    turns: Arc<TurnRegistry>,
    /// Per-run kill/pause/resume control handles (Unified Chat UI phase 6, D3). `pub(crate)`
    /// (not private): `run_control::control::resume_run` (same crate, different module) reaches
    /// `app.shutdown`/`app.tasks` directly, mirroring `launch.rs`'s existing access.
    pub(crate) run_control: Arc<RunControlRegistry>,
    /// Data-driven slash-command registry (Unified Chat UI phase 2, D1) — built-ins +
    /// authored + gate-filtered synthesized skills, unioned. `pub` (mirrors `db`/`kms`) so the
    /// mode layer (`src-tauri`) can clone the SAME handle into its own `AppState` for
    /// `list_slash_commands`, rather than maintaining a second registry.
    pub slash_registry: Arc<SlashRegistry>,
    /// OS-toast notifier (Unified Chat UI phase 7, D7) — the SAME `Arc` every launch path
    /// (`launch.rs`, `run_control::control::resume_run`, `trigger.rs` via `LaunchHandles`)
    /// threads into `spawn_run_event_bridge`. `pub(crate)`: only same-crate launch code needs it.
    pub(crate) notifier: Arc<dyn OsNotifier>,
    /// Cross-run toast burst coalescer (D7) — ONE instance for the process lifetime, shared by
    /// every run's bridge instance so a burst across CONCURRENT runs collapses correctly (a
    /// per-run instance would never see another run's recent toast). `pub(crate)`, same reason.
    pub(crate) toast_coalescer: Arc<ToastCoalescer>,
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
        // Unified Chat UI phase 6 (D3): constructed here (mirrors `turns`) so it can be handed
        // into `spawn_dispatch_loop` for `trigger.rs`'s launch path AND stored on `AppHandle` for
        // `launch.rs`/the mode layer — one registry shared by every launch path.
        let run_control = Arc::new(RunControlRegistry::new());

        let db_path = data_dir.join("haily.db");
        info!("DB: {}", db_path.display());
        let db = DbHandle::init(&db_path).await?;

        info!("KMS: loading HNSW index (dump if present, else rebuild from DB)…");
        let kms = KmsHandle::init(db.clone(), data_dir).await?;

        let db = Arc::new(db);
        let kms = Arc::new(kms);
        let llm_cfg = load_llm_config(&kms).await;

        // Harness Completion phase 4: move any plaintext connector secret already sitting
        // in `kms_preferences` into the OS keyring. M5a: headless/Session-0 never attempts
        // the keyring at all (DPAPI/secret-service are both unreliable there), so the
        // migration step is skipped outright rather than attempted-and-failed on every
        // boot. Idempotent either way — `migrate_from_db` is a no-op once the row already
        // holds the keyring marker (or `attempt_keyring` is false, since a headless boot
        // recording a fallback warning here would be noise: it never even tried).
        let credential_policy = if opts.attempt_keyring {
            CredentialPolicy::default()
        } else {
            CredentialPolicy::headless()
        };
        let credential_store = Arc::new(CredentialStore::new(Arc::clone(&db), credential_policy));
        if opts.attempt_keyring {
            if let Err(e) = credential_store.migrate_from_db(ODOO_API_KEY_PREF).await {
                // Never fatal: the DB row is left untouched on a failed migration (no data
                // loss — see `migrate_from_db`'s contract), so the connector simply keeps
                // reading the plaintext value until the next successful boot's attempt.
                warn!("credential migration for '{ODOO_API_KEY_PREF}' failed: {e:#}");
            }
        }

        // Phase 6 (M7a/M7b, backup credential posture): a scheduled backup taken before
        // the one known connector credential has migrated out of plaintext would retain
        // it in the copy unless scrubbed. Checked HERE (not in `haily-proactive`, which
        // sits below this crate and has no visibility into keyring state) right after
        // the migration attempt above — `Ok(None)`/a marker row means nothing
        // plaintext-bearing is left behind; a residual plaintext row (attempt_keyring
        // off, or a failed keyring write) means "not clean yet". A DB read error fails
        // closed (treated as not-clean) rather than risking an unscrubbed plaintext
        // backup on an inconclusive check.
        //
        // M7b: this bool no longer gates WHETHER a backup happens (that indefinitely
        // starved durability when the keyring is persistently unavailable) — it only
        // tells `haily_proactive::backup` whether to scrub `ODOO_API_KEY_PREF` out of
        // each backup's copy before promoting it. See that module's doc comment.
        let credential_migration_clean = match meta::get_preference(&db, ODOO_API_KEY_PREF).await {
            Ok(None) => true,
            Ok(Some(v)) => v.is_empty() || is_keyring_marker(&v),
            Err(e) => {
                warn!("credential posture check for backup gating failed: {e:#} — treating as not-clean");
                false
            }
        };

        // Validate the auto_approve allowlist against the same tool set the
        // orchestrator will build from. A destructive/exfil tool listed here is a
        // config error at boot — never silently ignored, never auto-approved.
        let auto_approve_raw = load_auto_approve(&kms).await;
        let auto_approve = validate_auto_approve(&auto_approve_raw, &ToolRegistry::build_v1())?;

        // Phase 2 (C1/M2): the credential store is the SOLE credential source `HttpExecutor`
        // consults for a manifest's declared `auth` — a raw-DB fallback on top of it would
        // silently defeat a deployment that disabled read-fallback (M5b), mirroring the same
        // "getter is authoritative" contract `OdooExecutor::read_key` already enforces.
        let orchestrator = Arc::new(
            Orchestrator::init(
                Arc::clone(&kms),
                Arc::clone(&db),
                llm_cfg,
                shutdown.child_token(),
                tasks.clone(),
                auto_approve,
                Some(Arc::clone(&credential_store)
                    as Arc<dyn haily_tools::connector::CredentialGetter>),
            )
            .await?,
        );

        info!(
            llm = orchestrator.llm_provider(),
            // Auto Model Routing R1 (phase 4): surfaces the live kill-switch state at
            // boot for operator visibility — the SAME `Arc<AtomicBool>` `set_preference`
            // flips live (see `routing_enabled_handle()`'s doc), read here only for the
            // log line, never re-derived.
            routing_enabled = orchestrator
                .routing_enabled_handle()
                .load(std::sync::atomic::Ordering::Acquire),
            "ready"
        );

        // Adapters are constructed by the caller before the orchestrator (and its
        // approval broker) exist, so the resolver is injected here — after `init`,
        // before `start_all` begins accepting requests — rather than at construction.
        let resolver = orchestrator.approval_resolver();
        let kill = orchestrator.kill_handle();
        // Phase 12: the ACP channel replays a session transcript on `session/load` from the
        // existing `messages` storage. Injected here (post-construction) like the resolver +
        // kill switch; a channel with no replay surface ignores it via the trait default.
        let transcript: Arc<dyn haily_types::SessionTranscript> = Arc::new(
            crate::session_transcript::DbSessionTranscript::new(Arc::clone(&db)),
        );
        // Mobile Thin-Client plan phase 3 amendment: `turns` (below) already exists at this
        // point (constructed earlier in this same function), so it's injected in the same
        // post-construction loop as the other three seams — only `MobileAdapter` overrides the
        // default no-op (see `haily-io::Adapter::set_turn_canceller`'s doc comment).
        let turn_canceller = Arc::clone(&turns) as Arc<dyn haily_types::TurnCanceller>;
        for adapter in &adapters {
            adapter.set_approval_resolver(Arc::clone(&resolver));
            adapter.set_kill_switch(Arc::clone(&kill));
            adapter.set_session_transcript(Arc::clone(&transcript));
            adapter.set_turn_canceller(Arc::clone(&turn_canceller));
        }

        let mut builder = AdapterManager::builder();
        for adapter in adapters {
            builder = builder.register(adapter);
        }
        let am = builder.build();
        // Mobile Thin-Client plan phase 2a review fix (m7): the manager can only be injected
        // back into adapters AFTER it exists, i.e. after `build()` — one line, mirrors the
        // resolver/kill/transcript injection loop just above but necessarily separate from it.
        am.wire_self_reference();

        // Unified Chat UI phase 2 (D1): built once at boot, then rebuilt lazily by the
        // dispatch loop itself (`SlashRegistry::ensure_fresh` polls `AuthoredRegistry::version()`
        // per request — no push hook, per the P02↔P08 interop contract).
        let slash_registry = Arc::new(SlashRegistry::new());
        slash_registry.rebuild(&kms, &db).await;

        // Unified Chat UI phase 7 (D7): `opts.os_notifier` is either the Tauri shell's real,
        // window-focus-aware implementation (constructed by `src-tauri` BEFORE this call) or the
        // `Default`'s `NoopNotifier` (CLI/headless/tests). `toast_coalescer` is constructed fresh
        // per process — one instance shared by every launch path via `AppHandle`/`LaunchHandles`.
        let notifier = Arc::clone(&opts.os_notifier);
        let toast_coalescer = Arc::new(ToastCoalescer::new());

        dispatch::spawn_dispatch_loop(
            am.clone(),
            Arc::clone(&orchestrator),
            shutdown.child_token(),
            tasks.clone(),
            Arc::clone(&turns),
            Arc::clone(&slash_registry),
            Arc::clone(&run_control),
            Arc::clone(&notifier),
            Arc::clone(&toast_coalescer),
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
                Arc::clone(&kms),
                am.clone(),
                shutdown.child_token(),
                tasks.clone(),
            );
        }

        // Phase 3: periodically purge action-journal rows past their retention window so
        // recorded PII (request_params/pre_state/post_state) is bounded. Registered on the
        // root tracker + selecting on shutdown, same contract as the watcher/daemon.
        watchers::spawn_journal_purge(Arc::clone(&db), shutdown.child_token(), tasks.clone());

        // Phase 6 ("Activate & Measure"): scheduled GFS backup — the durability guarantee
        // for the single `haily.db` file this whole app's memory lives in. Runs regardless
        // of `opts.enable_daemon`/`enable_watcher` (durability is not an optional feature
        // toggle the way the proactive daemon's notifications are).
        watchers::spawn_backup(
            Arc::clone(&db),
            data_dir.join("backups"),
            credential_migration_clean,
            vec![ODOO_API_KEY_PREF.to_string()],
            shutdown.child_token(),
            tasks.clone(),
        );

        // Phase 6 ("Pipeline Activation & Wiring"): periodic worktree reaper — reclaims
        // coding_workspaces whose owning pipeline run finished (or never linked to one and
        // went stale), plus crash-orphaned worktree directories with no matching row. Runs
        // regardless of `opts` (same rationale as `spawn_backup`: bounding disk usage from
        // launched runs is not an optional feature toggle).
        reaper::spawn_worktree_reaper(
            Arc::clone(&db),
            reaper::default_worktrees_root(),
            shutdown.child_token(),
            tasks.clone(),
        );

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
            credential_store,
            shutdown,
            tasks,
            turns,
            run_control,
            slash_registry,
            notifier,
            toast_coalescer,
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

    /// Shared handle to the run-control registry (Unified Chat UI phase 6, D3), for callers
    /// (e.g. the Tauri command layer) that want their own `Arc` clone — mirrors
    /// `turn_registry()`.
    pub fn run_control_registry(&self) -> Arc<RunControlRegistry> {
        Arc::clone(&self.run_control)
    }

    /// Shared OS-toast notifier (Unified Chat UI phase 7, D7), for callers (dispatch-layer
    /// `trigger.rs`, tests) that build their own [`crate::run_control::LaunchCtx`]/
    /// [`crate::trigger::LaunchHandles`] outside `launch.rs`'s own entrypoint.
    pub fn os_notifier(&self) -> Arc<dyn OsNotifier> {
        Arc::clone(&self.notifier)
    }

    /// Shared toast-burst coalescer (D7) — see [`Self::os_notifier`]'s doc.
    pub fn toast_coalescer(&self) -> Arc<ToastCoalescer> {
        Arc::clone(&self.toast_coalescer)
    }

    /// Snapshot of every in-flight tool approval across all channels (phase 11a) for the
    /// unified approvals queue. Delegates to the orchestrator's broker; each entry's
    /// `session_id` is the auth boundary a UI must respect when offering to resolve it.
    pub fn pending_approvals(&self) -> Vec<haily_core::PendingApproval> {
        self.orchestrator.pending_approvals()
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
