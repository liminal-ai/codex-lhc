//! Claim hand-back after capture workers stop.
//!
//! Each capture runtime releases its own database when `worker_loop` returns.
//! Process-wide release-all remains only at real process exit (CLI, standalone
//! app-server). Requires the lhc-rs pin that exports `release_held_claims_for`.

use std::path::Path;

use tracing::info;

use crate::gating::lhc_root;
use crate::session::thread_file_path;

/// Hand back claims held on one thread database. Call from that thread's
/// capture runtime after its worker loop has returned.
pub fn on_thread_unload(root: Option<&Path>, thread_id: &str) {
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
