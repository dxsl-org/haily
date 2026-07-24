//! OS-toast notification seam (Unified Chat UI phase 7, D7).
//!
//! `haily-app` backs the CLI/headless binary too, so it must not depend on `tauri` — but firing
//! a REAL OS notification needs a live window handle (for the focus/minimized gate). This trait
//! is the seam: the Tauri shell supplies the concrete implementation via
//! `BootstrapOptions::os_notifier`, constructed and injected BEFORE `AppHandle::bootstrap` runs
//! (unlike the resolver/kill/transcript seams in `bootstrap.rs`, which are injected AFTER — a
//! Tauri `AppHandle` already exists at the point `src-tauri`'s `setup()` calls `bootstrap()`, so
//! no post-construction step is needed here). Every other mode (CLI, headless, tests) gets
//! [`NoopNotifier`], the `Default` on `BootstrapOptions`.
use async_trait::async_trait;
use haily_types::RunEvent;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fires one OS-level toast. Implementations own BOTH the focus/minimized gate (only actually
/// showing anything while the window is unfocused/minimized — the in-app card/badge already
/// covers the focused case) and the underlying OS call. The caller ([`maybe_notify`]) only
/// guarantees this runs off the delivery path with a bounded timeout — never blocking, never
/// checked for success.
#[async_trait]
pub trait OsNotifier: Send + Sync {
    async fn notify(&self, title: &str, body: &str);
}

/// Default for every mode without a real window (CLI, headless, tests).
pub struct NoopNotifier;

#[async_trait]
impl OsNotifier for NoopNotifier {
    async fn notify(&self, _title: &str, _body: &str) {}
}

/// Preference key gating whether a toast is attempted at all (default-on when unset — read by
/// the caller, e.g. `watchers::spawn_run_event_bridge`, via the generic `meta` preference store;
/// there is no dedicated setter command, the existing generic `set_preference` Tauri command
/// already covers any string key).
pub const NOTIFICATIONS_ENABLED_PREF: &str = "ui.notifications_enabled";

/// Bound on how long a detached toast attempt may run before being abandoned (red-team MAJOR:
/// a hung OS notification center, a first-run permission prompt, or slow WinRT IPC must never
/// accumulate unbounded background work).
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum spacing between two toasts of the SAME kind — collapses a burst (several parallel
/// runs completing within the same window) into one OS toast rather than a flood. Mirrors the
/// per-kind cap philosophy of `haily_io::proactive_cards` without needing that module's
/// accumulation semantics (a toast is transient, never a persisted card).
const COALESCE_WINDOW: Duration = Duration::from_secs(5);

/// The three kinds this phase toasts on (D7) — every other `RunEvent` variant is a no-op here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ToastKind {
    RunComplete,
    ApprovalNeeded,
    RunPaused,
}

impl ToastKind {
    fn label(self) -> &'static str {
        match self {
            ToastKind::RunComplete => "run_complete",
            ToastKind::ApprovalNeeded => "approval_needed",
            ToastKind::RunPaused => "run_paused",
        }
    }

    fn slot(self) -> usize {
        match self {
            ToastKind::RunComplete => 0,
            ToastKind::ApprovalNeeded => 1,
            ToastKind::RunPaused => 2,
        }
    }
}

/// Build the (kind, title, body) triple for a toast-worthy event, or `None` for everything else.
/// Every interpolated value is fixed, small-vocabulary runner output (`outcome`/`reason` strings
/// the pipeline itself assigns) — never raw tool/LLM output, the same safety argument
/// `run-narration.ts`'s module doc makes for the in-app card; a notification body can surface on
/// the OS lock screen, so this must never carry `StageOutput.chunk`/`GateResult.decisive`/etc.
fn toast_for(event: &RunEvent) -> Option<(ToastKind, &'static str, String)> {
    match event {
        RunEvent::RunComplete { outcome, .. } => {
            let lower = outcome.to_lowercase();
            let failed = lower.contains("fail") || lower.contains("error");
            let title = if failed {
                "Haily — Tác vụ thất bại"
            } else {
                "Haily — Tác vụ hoàn tất"
            };
            Some((ToastKind::RunComplete, title, format!("Kết quả: {outcome}")))
        }
        RunEvent::ApprovalNeeded { .. } => Some((
            ToastKind::ApprovalNeeded,
            "Haily — Cần bạn phê duyệt",
            "Một tác vụ đang chờ bạn phê duyệt để tiếp tục.".to_string(),
        )),
        RunEvent::RunPaused { reason, .. } => Some((
            ToastKind::RunPaused,
            "Haily — Tác vụ đã tạm dừng",
            format!("Lý do: {reason}"),
        )),
        _ => None,
    }
}

/// Per-kind last-fired timestamp, shared across every run this process drives — so a burst of
/// same-kind events across CONCURRENT runs collapses to one toast per [`COALESCE_WINDOW`] rather
/// than one per run.
#[derive(Default)]
pub struct ToastCoalescer {
    last_fired: Mutex<[Option<Instant>; 3]>,
}

impl ToastCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` iff this kind hasn't fired within `COALESCE_WINDOW` — records the firing time as a
    /// side effect of returning `true`, so a call inside the window immediately after returns
    /// `false`.
    fn should_fire(&self, kind: ToastKind) -> bool {
        let mut slots = self.last_fired.lock().unwrap_or_else(|e| e.into_inner());
        let idx = kind.slot();
        let now = Instant::now();
        let fire = match slots[idx] {
            Some(last) => now.duration_since(last) >= COALESCE_WINDOW,
            None => true,
        };
        if fire {
            slots[idx] = Some(now);
        }
        fire
    }
}

/// Fire an OS toast for `event` if it is toast-worthy, enabled, and not coalesced away — ALWAYS
/// on a detached `tokio::spawn` with its own timeout (red-team MAJOR: must never be awaited
/// inline on the delivery bridge). `enabled` is the caller's already-read
/// [`NOTIFICATIONS_ENABLED_PREF`] (default-on when unset).
pub fn maybe_notify(
    notifier: &Arc<dyn OsNotifier>,
    coalescer: &Arc<ToastCoalescer>,
    enabled: bool,
    event: &RunEvent,
) {
    if !enabled {
        return;
    }
    let Some((kind, title, body)) = toast_for(event) else {
        return;
    };
    if !coalescer.should_fire(kind) {
        tracing::debug!(
            kind = kind.label(),
            "toast coalesced away (same-kind burst)"
        );
        return;
    }
    let notifier = Arc::clone(notifier);
    tokio::spawn(async move {
        if tokio::time::timeout(NOTIFY_TIMEOUT, notifier.notify(title, &body))
            .await
            .is_err()
        {
            tracing::warn!("OS notification timed out and was abandoned");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    struct SpyNotifier {
        tx: mpsc::Sender<(String, String)>,
    }

    #[async_trait]
    impl OsNotifier for SpyNotifier {
        async fn notify(&self, title: &str, body: &str) {
            let _ = self.tx.send((title.to_string(), body.to_string())).await;
        }
    }

    fn run(run_id: &str, outcome: &str) -> RunEvent {
        RunEvent::RunComplete {
            run_id: run_id.to_string(),
            outcome: outcome.to_string(),
        }
    }

    #[tokio::test]
    async fn fires_for_a_toast_worthy_event_when_enabled() {
        let (tx, mut rx) = mpsc::channel(4);
        let notifier: Arc<dyn OsNotifier> = Arc::new(SpyNotifier { tx });
        let coalescer = Arc::new(ToastCoalescer::new());

        maybe_notify(&notifier, &coalescer, true, &run("r1", "done"));

        let (title, body) = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("toast must fire")
            .expect("channel open");
        assert!(title.contains("hoàn tất"));
        assert!(body.contains("done"));
    }

    #[tokio::test]
    async fn never_fires_when_disabled() {
        let (tx, mut rx) = mpsc::channel(4);
        let notifier: Arc<dyn OsNotifier> = Arc::new(SpyNotifier { tx });
        let coalescer = Arc::new(ToastCoalescer::new());

        maybe_notify(&notifier, &coalescer, false, &run("r1", "done"));

        assert!(
            timeout(Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "a disabled pref must never fire a toast"
        );
    }

    #[tokio::test]
    async fn never_fires_for_a_non_toast_worthy_event() {
        let (tx, mut rx) = mpsc::channel(4);
        let notifier: Arc<dyn OsNotifier> = Arc::new(SpyNotifier { tx });
        let coalescer = Arc::new(ToastCoalescer::new());

        maybe_notify(
            &notifier,
            &coalescer,
            true,
            &RunEvent::StageStarted {
                run_id: "r1".into(),
                stage: "build".into(),
                tier: None,
            },
        );

        assert!(
            timeout(Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "StageStarted must never toast"
        );
    }

    #[tokio::test]
    async fn coalesces_a_same_kind_burst_across_different_runs() {
        let (tx, mut rx) = mpsc::channel(4);
        let notifier: Arc<dyn OsNotifier> = Arc::new(SpyNotifier { tx });
        let coalescer = Arc::new(ToastCoalescer::new());

        maybe_notify(&notifier, &coalescer, true, &run("r1", "done"));
        maybe_notify(&notifier, &coalescer, true, &run("r2", "done"));
        maybe_notify(&notifier, &coalescer, true, &run("r3", "failed"));

        timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("first toast must fire")
            .expect("channel open");
        assert!(
            timeout(Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "a same-kind burst within the coalesce window must collapse to one toast"
        );
    }

    #[tokio::test]
    async fn does_not_coalesce_across_different_kinds() {
        let (tx, mut rx) = mpsc::channel(4);
        let notifier: Arc<dyn OsNotifier> = Arc::new(SpyNotifier { tx });
        let coalescer = Arc::new(ToastCoalescer::new());

        maybe_notify(&notifier, &coalescer, true, &run("r1", "done"));
        maybe_notify(
            &notifier,
            &coalescer,
            true,
            &RunEvent::ApprovalNeeded {
                run_id: "r1".into(),
                approval_id: "a1".into(),
            },
        );

        for _ in 0..2 {
            timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("both kinds must fire independently")
                .expect("channel open");
        }
    }

    #[test]
    fn noop_notifier_compiles_and_does_nothing() {
        let _n: Arc<dyn OsNotifier> = Arc::new(NoopNotifier);
    }
}
