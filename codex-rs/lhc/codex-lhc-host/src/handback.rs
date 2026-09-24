//! Claim hand-back after capture workers stop.
//!
//! Scoped release on thread close/unload; one in-process server's databases
//! on that server's close; process-wide release-all only at real process
//! exit. Requires the lhc-rs pin that exports `release_held_claims_for`.

use std::path::Path;

use tracing::info;

use crate::gating::lhc_root;
use crate::session::thread_file_path;

/// Hand back claims held on one thread database. Call after that thread's
/// capture workers have stopped.
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

/// Hand back claims held on one in-process server's thread databases.
///
/// Call after that server's threads have been asked to stop. Other live
/// clients in the same process keep their claims. Process-wide
/// [`on_process_shutdown`] is only for real process exit.
pub fn on_server_shutdown<'a, I>(root: Option<&Path>, thread_ids: I)
where
    I: IntoIterator<Item = &'a str>,
{
    for thread_id in thread_ids {
        on_thread_unload(root, thread_id);
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
