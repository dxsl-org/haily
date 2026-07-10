//! `Manager` — scope-keyed sandbox pooling (goclaw `sandbox.Manager` pattern).
//!
//! A WSL2 distro (and any real isolation primitive) is expensive to boot. The Manager keeps
//! ONE sandbox per [`ScopeKey`] and reuses it across turns in that scope, rather than spawning
//! per exec. Backend selection picks the strongest available for the host and fails safe to
//! [`NullSandbox`] — which forces first-exec approval — never to a silent unsandboxed run.

use super::config::{SandboxConfig, ScopeKey};
use super::null::NullSandbox;
use super::sandbox::Sandbox;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Lightweight pool telemetry (spike + observability).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManagerStats {
    /// Sandboxes constructed (pool misses).
    pub created: usize,
    /// Reuses served from the pool (pool hits).
    pub pool_hits: usize,
    /// Currently pooled (live) sandboxes.
    pub active: usize,
}

pub struct Manager {
    default_config: SandboxConfig,
    pool: Mutex<HashMap<ScopeKey, Arc<dyn Sandbox>>>,
    // Atomics, NOT nested `Mutex`es: `get()` bumps a counter while holding the pool lock, so a
    // second lock here would invert `stats()`'s order and could deadlock. Atomics have no order.
    created: AtomicUsize,
    pool_hits: AtomicUsize,
}

impl Manager {
    pub fn new(default_config: SandboxConfig) -> Self {
        Self {
            default_config,
            pool: Mutex::new(HashMap::new()),
            created: AtomicUsize::new(0),
            pool_hits: AtomicUsize::new(0),
        }
    }

    pub fn default_config(&self) -> &SandboxConfig {
        &self.default_config
    }

    /// Get (or create) the sandbox for `key`. Reuses the pooled instance across turns in the
    /// same scope; the per-exec work root travels in the `ExecRequest`, not the sandbox, so one
    /// pooled sandbox serves many work roots.
    pub fn get(&self, key: ScopeKey) -> Arc<dyn Sandbox> {
        let mut pool = self.pool.lock().expect("sandbox pool poisoned");
        if let Some(existing) = pool.get(&key) {
            self.pool_hits.fetch_add(1, Ordering::Relaxed);
            return Arc::clone(existing);
        }
        let sb = Self::select_backend();
        pool.insert(key, Arc::clone(&sb));
        self.created.fetch_add(1, Ordering::Relaxed);
        sb
    }

    /// Drop the pooled sandbox for `key` (its `Drop` tears down any backend resources).
    pub fn release(&self, key: &ScopeKey) {
        self.pool.lock().expect("sandbox pool poisoned").remove(key);
    }

    /// Drop every pooled sandbox.
    pub fn release_all(&self) {
        self.pool.lock().expect("sandbox pool poisoned").clear();
    }

    pub fn stats(&self) -> ManagerStats {
        // Single lock (pool) only; counters are atomic — no lock-ordering surface with `get()`.
        ManagerStats {
            created: self.created.load(Ordering::Relaxed),
            pool_hits: self.pool_hits.load(Ordering::Relaxed),
            active: self.pool.lock().expect("sandbox pool poisoned").len(),
        }
    }

    /// Pick the strongest available backend for this host, failing safe to `NullSandbox`.
    /// Windows → the managed WSL2 distro if provisioned (`HAILY_WSL_DISTRO`), else Null.
    /// macOS/Linux → Null in the gate phase (native `exec` lands with those platforms' CI).
    fn select_backend() -> Arc<dyn Sandbox> {
        #[cfg(windows)]
        {
            if let Some(wsl) = super::wsl2::Wsl2Sandbox::detect() {
                return Arc::new(wsl);
            }
        }
        Arc::new(NullSandbox::new())
    }
}

impl Default for Manager {
    fn default() -> Self {
        Self::new(SandboxConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_scope_reuses_one_sandbox() {
        let mgr = Manager::default();
        let key = ScopeKey::session("s1");
        let a = mgr.get(key.clone());
        let b = mgr.get(key.clone());
        assert!(Arc::ptr_eq(&a, &b), "same scope must return the pooled sandbox");
        let stats = mgr.stats();
        assert_eq!(stats.created, 1);
        assert_eq!(stats.pool_hits, 1);
        assert_eq!(stats.active, 1);
    }

    #[test]
    fn distinct_scopes_get_distinct_sandboxes() {
        let mgr = Manager::default();
        let a = mgr.get(ScopeKey::session("s1"));
        let b = mgr.get(ScopeKey::agent("a1"));
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(mgr.stats().created, 2);
    }

    #[test]
    fn release_tears_down() {
        let mgr = Manager::default();
        let key = ScopeKey::shared();
        let _ = mgr.get(key.clone());
        assert_eq!(mgr.stats().active, 1);
        mgr.release(&key);
        assert_eq!(mgr.stats().active, 0);

        // After release, a get re-creates (miss, not hit).
        let _ = mgr.get(key);
        assert_eq!(mgr.stats().created, 2);
        mgr.release_all();
        assert_eq!(mgr.stats().active, 0);
    }
}
