mod cross_domain;
mod dnd;
mod morning_brief;
mod reminders;

use haily_db::DbHandle;
use haily_io::AdapterManager;
use std::sync::Arc;

/// Proactive background engine — fires morning briefs, reminders, and cross-domain alerts.
///
/// `start()` spawns three independent tokio tasks and returns immediately.
pub struct ProactiveDaemon {
    db: Arc<DbHandle>,
    am: AdapterManager,
}

impl ProactiveDaemon {
    pub fn new(db: Arc<DbHandle>, am: AdapterManager) -> Self {
        Self { db, am }
    }

    /// Spawn all background loops. Non-blocking — each loop runs as a detached task.
    pub fn start(self) {
        let db = self.db;
        let am = self.am;

        tokio::spawn(morning_brief::loop_forever(db.clone(), am.clone()));
        tokio::spawn(reminders::poll_loop(db.clone(), am.clone()));
        tokio::spawn(cross_domain::alert_loop(db, am));
    }
}
