//! Atomic rollout rewrite (slice C of the rollout rework).
//!
//! # Generation retention scheme
//!
//! For a live rollout path `P` (for example
//! `…/sessions/YYYY/MM/DD/rollout-….jsonl`):
//!
//! | Role | Path |
//! |---|---|
//! | Active generation | `P` |
//! | Prior generation (exactly one) | `P` with suffix `.prev` (i.e. `P.prev`) |
//! | In-progress rewrite | `P` with suffix `.rewrite-tmp` |
//!
//! Steps (any error before the final rename leaves `P` untouched and
//! authoritative; the session continues and the next compact retries):
//!
//! 1. Write the full materialized sequence to `P.rewrite-tmp`.
//! 2. `fsync` the temp file.
//! 3. `fsync` the parent directory (durability of the directory entry).
//! 4. If `P.prev` exists, remove it (retain exactly one prior generation).
//! 5. If `P` exists, `rename(P → P.prev)`.
//! 6. `rename(P.rewrite-tmp → P)`.
//! 7. Caller reopens the append-mode recorder handle so it points at the new
//!    inode (an unreopened fd silently follows the orphaned prior file).
//!
//! There is **no** append fallback on failure.

use chrono::SecondsFormat;
use chrono::Utc;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::RolloutItem;
use serde::Serialize;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Error as IoError;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// Injectable failpoints for crash-injection tests (`cfg(test)` / `test-util`).
///
/// Values match [`SwapFailpoint`] discriminants. `0` = no injection.
/// Guarded by [`SWAP_FAILPOINT_LOCK`] so parallel tests cannot race the atomic.
#[cfg(any(test, feature = "test-util"))]
pub static SWAP_FAILPOINT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Serializes failpoint arming across tests (global atomic).
#[cfg(any(test, feature = "test-util"))]
pub static SWAP_FAILPOINT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Crash-injection points between atomic-swap steps.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapFailpoint {
    None = 0,
    /// After the temp file body is fully written (before fsync).
    PostTempWrite = 1,
    /// After temp-file fsync (before directory fsync / renames).
    PostFsync = 2,
    /// After `P → P.prev` (before `tmp → P`).
    PostOldRename = 3,
    /// After `tmp → P` (before the caller reopens the recorder).
    PostNewRenamePreReopen = 4,
}

#[cfg(any(test, feature = "test-util"))]
impl SwapFailpoint {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::PostTempWrite,
            2 => Self::PostFsync,
            3 => Self::PostOldRename,
            4 => Self::PostNewRenamePreReopen,
            _ => Self::None,
        }
    }
}

/// Paths used by one rewrite of `rollout_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapPaths {
    pub active: PathBuf,
    pub prev: PathBuf,
    pub temp: PathBuf,
}

impl SwapPaths {
    pub fn for_rollout(rollout_path: &Path) -> Self {
        let mut prev = rollout_path.as_os_str().to_os_string();
        prev.push(".prev");
        let mut temp = rollout_path.as_os_str().to_os_string();
        temp.push(".rewrite-tmp");
        Self {
            active: rollout_path.to_path_buf(),
            prev: PathBuf::from(prev),
            temp: PathBuf::from(temp),
        }
    }
}

/// Atomically rewrite `rollout_path` with `items`.
///
/// On success the active path holds the new generation and at most one prior
/// generation sits at `*.prev`. On any error before the final rename, the
/// previous active file (if any) is left intact.
pub fn atomic_rewrite_rollout(rollout_path: &Path, items: &[RolloutItem]) -> std::io::Result<()> {
    let paths = SwapPaths::for_rollout(rollout_path);
    let parent = paths.active.parent().ok_or_else(|| {
        IoError::other(format!(
            "rollout path has no parent: {}",
            paths.active.display()
        ))
    })?;

    // 1. Write full sequence to temp.
    write_rollout_jsonl(&paths.temp, items)?;
    maybe_fail(SwapFailpoint::PostTempWrite)?;

    // 2. fsync temp file.
    {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&paths.temp)?;
        file.sync_all()?;
    }
    maybe_fail(SwapFailpoint::PostFsync)?;

    // 3. fsync directory (so the temp dirent is durable before rename).
    fsync_dir(parent)?;

    // 4. Drop previous prior generation (retain exactly one).
    if paths.prev.exists() {
        std::fs::remove_file(&paths.prev)?;
    }

    // 5. Move current active → prior (if present).
    if paths.active.exists() {
        std::fs::rename(&paths.active, &paths.prev)?;
    }
    maybe_fail(SwapFailpoint::PostOldRename)?;

    // 6. Move temp → active (atomic replace of the live path).
    std::fs::rename(&paths.temp, &paths.active)?;
    // Directory fsync after the final rename so the new dirent is durable.
    fsync_dir(parent)?;
    maybe_fail(SwapFailpoint::PostNewRenamePreReopen)?;

    Ok(())
}

/// Model-visible history that resume rebuilds from a materialized rollout:
/// `Compacted.replacement_history` (bands) followed by post-boundary
/// `ResponseItem`s (and inter-agent communications converted to model input).
///
/// Dual-format: when multiple `Compacted` records are present (pre-rework
/// appended shape), only the **newest** boundary's bands + the tail after that
/// boundary contribute. Earlier generations are ignored.
///
/// Must equal the in-memory history installed at compact (slice C alignment)
/// for rewritten (single-boundary) files.
pub fn history_from_materialized_items(items: &[RolloutItem]) -> Vec<ResponseItem> {
    // Find newest Compacted index.
    let mut last_idx: Option<usize> = None;
    let mut bands: Option<Vec<ResponseItem>> = None;
    for (i, item) in items.iter().enumerate() {
        if let RolloutItem::Compacted(CompactedItem {
            replacement_history,
            ..
        }) = item
        {
            last_idx = Some(i);
            bands = replacement_history.clone();
        }
    }
    let mut tail: Vec<ResponseItem> = Vec::new();
    let start = last_idx.map(|i| i + 1).unwrap_or(0);
    // No Compacted → entire file's ResponseItems are the model stream
    // (pre-first-compact session).
    let scan = if last_idx.is_some() {
        &items[start..]
    } else {
        items
    };
    for item in scan {
        match item {
            RolloutItem::ResponseItem(response_item) => {
                tail.push(response_item.clone());
            }
            RolloutItem::InterAgentCommunication(communication) => {
                tail.push(communication.to_model_input_item());
            }
            _ => {}
        }
    }
    let mut out = bands.unwrap_or_default();
    out.extend(tail);
    out
}

/// Model-context size baseline for the reduction self-check (F-L4): estimate
/// tokens of [`history_from_materialized_items`] for a rollout file.
pub fn model_context_token_estimate_from_rollout_items(items: &[RolloutItem]) -> i64 {
    use crate::estimate_response_items_tokens;
    estimate_response_items_tokens(&history_from_materialized_items(items))
}

/// Parse a rollout JSONL file into items (best-effort, skips corrupt lines).
pub fn parse_rollout_items(path: &Path) -> std::io::Result<Vec<RolloutItem>> {
    use std::io::BufRead;
    let file = File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut items = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match serde_json::from_value::<codex_protocol::protocol::RolloutLine>(value) {
            Ok(rollout_line) => items.push(rollout_line.item),
            Err(_) => continue,
        }
    }
    Ok(items)
}

/// RAII arm for a swap failpoint. Holds the global lock and clears on drop.
#[cfg(any(test, feature = "test-util"))]
pub struct SwapFailpointGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(any(test, feature = "test-util"))]
impl SwapFailpointGuard {
    pub fn arm(point: SwapFailpoint) -> Self {
        let lock = SWAP_FAILPOINT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SWAP_FAILPOINT.store(point as u8, std::sync::atomic::Ordering::SeqCst);
        Self { _lock: lock }
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Drop for SwapFailpointGuard {
    fn drop(&mut self) {
        SWAP_FAILPOINT.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(any(test, feature = "test-util"))]
pub fn set_swap_failpoint(point: SwapFailpoint) {
    SWAP_FAILPOINT.store(point as u8, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(any(test, feature = "test-util"))]
pub fn clear_swap_failpoint() {
    SWAP_FAILPOINT.store(0, std::sync::atomic::Ordering::SeqCst);
}

fn maybe_fail(point: SwapFailpoint) -> std::io::Result<()> {
    #[cfg(any(test, feature = "test-util"))]
    {
        let active =
            SwapFailpoint::from_u8(SWAP_FAILPOINT.load(std::sync::atomic::Ordering::SeqCst));
        if active == point && active != SwapFailpoint::None {
            return Err(IoError::other(format!(
                "injected swap failpoint: {point:?}"
            )));
        }
    }
    #[cfg(not(any(test, feature = "test-util")))]
    {
        let _ = point;
    }
    Ok(())
}

fn write_rollout_jsonl(path: &Path, items: &[RolloutItem]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    for item in items {
        write_one_line(&mut file, item)?;
    }
    file.flush()?;
    Ok(())
}

fn write_one_line(file: &mut File, item: &RolloutItem) -> std::io::Result<()> {
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

    #[derive(Serialize)]
    struct Line<'a> {
        timestamp: String,
        #[serde(flatten)]
        item: &'a RolloutItem,
    }

    let mut json = serde_json::to_string(&Line { timestamp, item })
        .map_err(|e| IoError::other(format!("serialize rollout item: {e}")))?;
    json.push('\n');
    file.write_all(json.as_bytes())
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    // Directory fsync is best-effort on platforms that reject it; the renames
    // themselves remain atomic for crash consistency of the active path.
    match File::open(dir) {
        Ok(dir_file) => {
            if let Err(err) = dir_file.sync_all() {
                // Windows / some FS: EISDIR / InvalidInput — not fatal for the swap.
                if err.kind() == std::io::ErrorKind::InvalidInput
                    || err.raw_os_error() == Some(21) /* EISDIR */
                    || err.raw_os_error() == Some(22)
                /* EINVAL */
                {
                    return Ok(());
                }
                return Err(err);
            }
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[path = "rollout_swap_tests.rs"]
mod tests;
