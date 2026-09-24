//! Claim hand-back after capture workers stop.
//!
//! Each capture runtime releases its own database when `worker_loop` returns.
//! Process-wide release-all remains only at real process exit (CLI, standalone
//! app-server). Requires the lhc-rs pin that exports `release_held_claims_for`.

use std::path::Path;
use std::time::Duration;

use tracing::info;
use tracing::warn;

use crate::gating::lhc_root;
use crate::session::thread_file_path;

/// SDK `release_held_claims_for` waits up to 2s (`PRAGMA busy_timeout = 2000`).
pub(crate) const HANDBACK_BOUND: Duration = Duration::from_secs(2);

/// Hand back claims held on one thread database. Call from that thread's
/// capture runtime after its worker loop has returned.
pub fn on_thread_unload(root: Option<&Path>, thread_id: &str) {
    on_thread_unload_within(root, thread_id, HANDBACK_BOUND);
}

/// Like [`on_thread_unload`], but skip SQLite if `remaining` cannot cover the
/// SDK's 2s busy wait. Lease expiry is the fallback; the caller still releases
/// the live-path reservation.
pub(crate) fn on_thread_unload_within(root: Option<&Path>, thread_id: &str, remaining: Duration) {
    if remaining < HANDBACK_BOUND {
        warn!(
            thread_id,
            remaining_ms = remaining.as_millis(),
            "LHC: hand-back skipped; claims left to expire"
        );
        return;
    }
    let root = root.map(Path::to_path_buf).unwrap_or_else(lhc_root);
    let path = thread_file_path(&root, thread_id);
    let Some(path) = path.to_str() else {
        return;
    };
    let released = lhc::release_held_claims_for(path);
    if released > 0 {
        info!(
            thread_id,
            released, "LHC: handed back held claims for thread"
        );
    }
}

/// Hand back every still-held claim. Call once at coordinated process shutdown
/// after claim admission has stopped and workers have settled or cancelled.
pub fn on_process_shutdown() {
    let released = lhc::release_held_claims();
    if released > 0 {
        info!(released, "LHC: handed back held claims at process shutdown");
    }
}

#[cfg(test)]
#[path = "handback_tests.rs"]
mod tests;
