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
//!
//! # Interrupted-swap reconciliation
//!
//! An error out of [`atomic_rewrite_rollout`] does not by itself say which
//! generation is active: steps 1–5 leave `P` (old) authoritative, an error
//! between steps 5 and 6 leaves *no* active generation (old at `P.prev`, new
//! at `P.rewrite-tmp`), and an error after step 6 (directory fsync, or the
//! caller's post-swap hook) leaves the **new** generation active while the
//! caller's in-memory state is still old. [`classify_swap_state`] reads the
//! actual on-disk state and [`reconcile_interrupted_swap`] establishes exactly
//! one authoritative active generation before the caller decides anything:
//! it finishes an interrupted swap (`tmp → P`) when the new generation is
//! complete, restores `P.prev → P` when it is not, and reports a state it
//! cannot repair instead of guessing. It runs synchronously in the caller's
//! one-writer context (the compact arm that owns the thread), never from a
//! second writer.
//!
//! Both generations are identified **exactly** ([`SwapGenerations`]), never
//! by "parses and contains the boundary":
//!
//! * the **old** generation is the exact byte content of the authoritative
//!   active file immediately before the swap (captured by the caller after
//!   its final flush). `P.prev` is that same inode renamed, so a byte-exact
//!   match is the only proof that a file *is* the prior generation; any
//!   extra, missing, or altered row fails it;
//! * the **new** generation is the exact ordered wire content the swap wrote
//!   (`items`), proven by [`strict_read_generation`]: every line must be a
//!   complete JSON rollout line, nothing is skipped, the row count must
//!   match, each row's item object must equal the expected item's wire
//!   form, and the envelope must be exactly what [`write_rollout_jsonl`]
//!   emits — ordinals derived by the same [`RolloutOrdinalState::for_rewrite`]
//!   (none for legacy output, exact contiguous values for paginated output),
//!   one valid generated (UUID v4) `rollout_generation_id` on every
//!   `SessionMeta` row and on no other row, and an RFC 3339 timestamp (the
//!   value is nondeterministic; the contract is not). The tolerant
//!   [`parse_rollout_items`] (which skips rows it cannot read) is an
//!   operational reader and is never used to establish authority.
//!
//! Anything that proves neither is `Unknown`/`Unreconciled`: it is never
//! promoted, restored, or treated as old. A repair rename (`tmp → P` or
//! `P.prev → P`) is only reported as established when the parent directory
//! sync after it succeeds; a failed sync is reported as `Unreconciled` with
//! the exact state, because the rename's durability is unproven.

use chrono::SecondsFormat;
use chrono::Utc;
use codex_history::CompactedItem;
use codex_history::ROLLOUT_GENERATION_ID_FIELD;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_history::RolloutOrdinalState;
use codex_protocol::models::ResponseItem;
use serde::Serialize;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Error as IoError;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

// Injectable failpoints for crash-injection tests (`cfg(test)` / `test-util`).
//
// A bit mask over [`SwapFailpoint`] discriminants (`1 << discriminant`); `0`
// = no injection. A mask (not a single point) so a swap-stage failure and a
// reconciliation-repair failure can be armed together. Thread-local so
// parallel tests cannot inject failures into unrelated swaps.
#[cfg(any(test, feature = "test-util"))]
std::thread_local! {
    static SWAP_FAILPOINT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

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
    /// Inside [`reconcile_interrupted_swap`]: the parent-directory sync after
    /// a repair rename (`tmp → P` or `P.prev → P`) fails.
    ReconcileDirSync = 5,
}

#[cfg(any(test, feature = "test-util"))]
impl SwapFailpoint {
    fn bit(self) -> u8 {
        match self {
            Self::None => 0,
            point => 1u8 << (point as u8),
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

/// Which generation is active on disk after an interrupted rewrite, as
/// classified by [`classify_swap_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapState {
    /// `P` holds the old generation (steps 1–5 failed, or never ran).
    OldActive,
    /// `P` holds the new generation (steps 1–6 completed; the error came from
    /// the directory fsync after the final rename or from the caller's
    /// post-swap hook).
    NewActive,
    /// `P` is absent: the old generation was moved to `P.prev` and the new
    /// generation (if complete) sits at `P.rewrite-tmp` (failure between
    /// steps 5 and 6).
    NoActive {
        prev_exists: bool,
        temp_exists: bool,
    },
    /// `P` exists but proves neither the exact old nor the exact new
    /// generation (torn, foreign, or altered content). Never repaired
    /// automatically.
    Unknown { detail: String },
}

/// Exact identities of the two generations an interrupted swap moved between.
#[derive(Debug, Clone, Copy)]
pub struct SwapGenerations<'a> {
    /// Exact bytes of the authoritative active file immediately before the
    /// swap (after the caller's final flush); `None` when no active file
    /// existed. `P.prev` is that inode renamed, so only a byte-exact match
    /// proves a file is the prior generation.
    pub prior_bytes: Option<&'a [u8]>,
    /// The ordered items the swap wrote — the new generation's wire content.
    pub new_items: &'a [RolloutItem],
}

/// Envelope keys the writer adds around each item's own wire object.
const LINE_ENVELOPE_KEYS: [&str; 3] = ["timestamp", "ordinal", ROLLOUT_GENERATION_ID_FIELD];

/// One strictly read rollout line: the validated envelope and the item's
/// own wire object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictRow {
    /// RFC 3339 timestamp as written (validated, not compared).
    pub timestamp: String,
    /// Top-level ordinal, when present (a JSON unsigned integer).
    pub ordinal: Option<u64>,
    /// `rollout_generation_id`, when present (a JSON string).
    pub generation_id: Option<String>,
    /// The item's wire object with the envelope keys removed.
    pub item: serde_json::Value,
}

/// Strict, authority-grade read of one rollout generation: the file must be
/// UTF-8, newline-terminated, and every line must be one complete JSON
/// rollout line (envelope + item) — nothing is skipped or tolerated. The
/// envelope is validated per line (RFC 3339 timestamp, unsigned-integer
/// ordinal, string generation id); whether the envelope is the one the
/// writer would have emitted for a given generation is
/// [`proves_new_generation`]'s job. Returns the rows in file order, or the
/// first violation with its line number.
pub fn strict_read_generation(path: &Path) -> Result<Vec<StrictRow>, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("unreadable: {err}"))?;
    let text = std::str::from_utf8(&bytes).map_err(|err| format!("not UTF-8: {err}"))?;
    if text.is_empty() {
        return Err("empty file".to_string());
    }
    if !text.ends_with('\n') {
        return Err("last line is not newline-terminated (torn write)".to_string());
    }
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line.trim().is_empty() {
            return Err(format!("line {number}: blank line"));
        }
        // The typed decode validates the envelope types and the item shape…
        let typed = serde_json::from_str::<RolloutLine>(line)
            .map_err(|err| format!("line {number}: not a rollout line: {err}"))?;
        chrono::DateTime::parse_from_rfc3339(&typed.timestamp)
            .map_err(|err| format!("line {number}: timestamp is not RFC 3339: {err}"))?;
        // …and the raw object is what gets compared, so an unknown or extra
        // key (which a typed decode would silently drop) still fails.
        let mut value: serde_json::Value =
            serde_json::from_str(line).map_err(|err| format!("line {number}: not JSON: {err}"))?;
        let Some(object) = value.as_object_mut() else {
            return Err(format!("line {number}: not a JSON object"));
        };
        let generation_id = match object.get(ROLLOUT_GENERATION_ID_FIELD) {
            None => None,
            Some(serde_json::Value::String(id)) => Some(id.clone()),
            Some(other) => {
                return Err(format!(
                    "line {number}: {ROLLOUT_GENERATION_ID_FIELD} is not a string: {other}"
                ));
            }
        };
        for key in LINE_ENVELOPE_KEYS {
            object.remove(key);
        }
        rows.push(StrictRow {
            timestamp: typed.timestamp,
            ordinal: typed.ordinal,
            generation_id,
            item: value,
        });
    }
    Ok(rows)
}

/// The exact envelope [`write_rollout_jsonl`] emits for `items`: the ordinal
/// of every row (from the same [`RolloutOrdinalState::for_rewrite`] the writer
/// uses — none for legacy output, contiguous values for paginated output) and
/// whether the row carries the generation id (`SessionMeta` rows only).
fn expected_envelope(items: &[RolloutItem]) -> Result<Vec<(Option<u64>, bool)>, String> {
    let mut ordinal_state = ordinal_state_for_items(items)
        .map_err(|err| format!("expected new generation has no valid ordinal plan: {err}"))?;
    let mut envelope = Vec::with_capacity(items.len());
    for item in items {
        let ordinal = ordinal_state
            .current()
            .map_err(|err| format!("expected new generation ordinal plan: {err}"))?;
        envelope.push((ordinal, matches!(item, RolloutItem::SessionMeta(_))));
        ordinal_state.advance();
    }
    Ok(envelope)
}

/// A generation id is exactly what the writer emits: a UUID v4 string.
fn is_generated_identity(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|uuid| uuid.get_version() == Some(uuid::Version::Random))
}

/// Prove `path` is exactly the new generation `items` (see
/// [`strict_read_generation`]): same row count, same order, each row's item
/// wire object equal to the expected item's wire form, and the envelope the
/// writer emits for exactly these items — every ordinal equal to the
/// [`RolloutOrdinalState::for_rewrite`] plan (absent on legacy output), one
/// generated identity shared by every `SessionMeta` row and present on no
/// other row.
pub fn proves_new_generation(path: &Path, items: &[RolloutItem]) -> Result<(), String> {
    let rows = strict_read_generation(path)?;
    if rows.len() != items.len() {
        return Err(format!(
            "row count {} != expected new generation {}",
            rows.len(),
            items.len()
        ));
    }
    let envelope = expected_envelope(items)?;
    let mut generation_id: Option<&str> = None;
    for (index, ((row, expected), (expected_ordinal, carries_generation_id))) in
        rows.iter().zip(items.iter()).zip(envelope).enumerate()
    {
        let number = index + 1;
        let expected = serde_json::to_value(expected)
            .map_err(|err| format!("expected item {index} unserializable: {err}"))?;
        if row.item != expected {
            return Err(format!(
                "line {number}: item differs from the expected new generation"
            ));
        }
        if row.ordinal != expected_ordinal {
            return Err(format!(
                "line {number}: ordinal {:?} != expected {:?}",
                row.ordinal, expected_ordinal
            ));
        }
        match (row.generation_id.as_deref(), carries_generation_id) {
            (None, false) => {}
            (Some(id), false) => {
                return Err(format!(
                    "line {number}: {ROLLOUT_GENERATION_ID_FIELD} {id:?} on a non-SessionMeta row"
                ));
            }
            (None, true) => {
                return Err(format!(
                    "line {number}: SessionMeta row without {ROLLOUT_GENERATION_ID_FIELD}"
                ));
            }
            (Some(id), true) => {
                if !is_generated_identity(id) {
                    return Err(format!(
                        "line {number}: {ROLLOUT_GENERATION_ID_FIELD} {id:?} is not a generated identity"
                    ));
                }
                match generation_id {
                    None => generation_id = Some(id),
                    Some(first) if first == id => {}
                    Some(first) => {
                        return Err(format!(
                            "line {number}: {ROLLOUT_GENERATION_ID_FIELD} {id:?} differs from {first:?}"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Prove `path` is exactly the prior generation: byte-identical to the
/// authoritative active file captured before the swap.
fn proves_old_generation(path: &Path, prior_bytes: Option<&[u8]>) -> Result<(), String> {
    let Some(expected) = prior_bytes else {
        return Err("no prior generation existed before the swap".to_string());
    };
    let bytes = std::fs::read(path).map_err(|err| format!("unreadable: {err}"))?;
    if bytes.len() != expected.len() {
        return Err(format!(
            "{} bytes != prior generation {} bytes",
            bytes.len(),
            expected.len()
        ));
    }
    if bytes != expected {
        return Err("content differs from the prior generation".to_string());
    }
    Ok(())
}

/// What [`reconcile_interrupted_swap`] established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapReconciliation {
    /// The old generation is (still) the single active generation; the
    /// caller keeps its current in-memory state and may retry later.
    OldActive,
    /// The new generation is the single active generation — either it already
    /// was (`finished_here == false`) or the interrupted `tmp → P` rename was
    /// completed here (`finished_here == true`). The caller must complete its
    /// matching in-memory install; it must not roll the compact back.
    NewActive { finished_here: bool },
    /// The old generation was restored to `P` from `P.prev` because the new
    /// generation could not be made active. Retry later.
    RestoredOld { detail: String },
    /// No single authoritative generation could be established. The caller
    /// must not allow further sampling on this rollout until a human or the
    /// next-open reconciliation resolves it; `detail` names the exact state.
    Unreconciled { detail: String },
}

/// Read the on-disk swap state of `rollout_path`. The active file is old only
/// when it proves the exact prior generation and new only when it proves the
/// exact new generation ([`SwapGenerations`]); any other content is
/// `Unknown`.
pub fn classify_swap_state(rollout_path: &Path, generations: SwapGenerations<'_>) -> SwapState {
    let paths = SwapPaths::for_rollout(rollout_path);
    if !paths.active.exists() {
        return SwapState::NoActive {
            prev_exists: paths.prev.exists(),
            temp_exists: paths.temp.exists(),
        };
    }
    let old_reason = match proves_old_generation(&paths.active, generations.prior_bytes) {
        Ok(()) => return SwapState::OldActive,
        Err(reason) => reason,
    };
    let new_reason = match proves_new_generation(&paths.active, generations.new_items) {
        Ok(()) => return SwapState::NewActive,
        Err(reason) => reason,
    };
    SwapState::Unknown {
        detail: format!(
            "active rollout {} proves neither generation (old: {old_reason}; new: {new_reason})",
            paths.active.display()
        ),
    }
}

/// Sync `parent` after a repair rename; a failure means the rename's
/// durability is unproven.
fn sync_repair(parent: Option<&Path>) -> std::io::Result<()> {
    maybe_fail(SwapFailpoint::ReconcileDirSync)?;
    match parent {
        Some(parent) => fsync_dir(parent),
        None => Ok(()),
    }
}

/// Establish exactly one authoritative active generation after
/// [`atomic_rewrite_rollout`] returned an error (see the module docs).
///
/// Only the two rename edges are repaired, and only onto proven files:
/// `tmp → P` is finished when the temp file proves the exact new generation;
/// otherwise `P.prev → P` restores the prior generation when `P.prev` proves
/// it byte-exactly. A repair counts only once the directory sync after it
/// succeeds. Every other state is reported, never guessed.
pub fn reconcile_interrupted_swap(
    rollout_path: &Path,
    generations: SwapGenerations<'_>,
) -> SwapReconciliation {
    let paths = SwapPaths::for_rollout(rollout_path);
    match classify_swap_state(rollout_path, generations) {
        SwapState::OldActive => SwapReconciliation::OldActive,
        SwapState::NewActive => SwapReconciliation::NewActive {
            finished_here: false,
        },
        SwapState::Unknown { detail } => SwapReconciliation::Unreconciled { detail },
        SwapState::NoActive {
            prev_exists,
            temp_exists,
        } => {
            let parent = paths.active.parent();
            // Finish the intended swap only onto a proven new generation.
            let temp_proof = if temp_exists {
                proves_new_generation(&paths.temp, generations.new_items)
            } else {
                Err("absent".to_string())
            };
            let mut temp_disposition = match temp_proof {
                Ok(()) => match std::fs::rename(&paths.temp, &paths.active) {
                    Ok(()) => {
                        return match sync_repair(parent) {
                            Ok(()) => SwapReconciliation::NewActive {
                                finished_here: true,
                            },
                            Err(err) => SwapReconciliation::Unreconciled {
                                detail: format!(
                                    "finished tmp → {} but the directory sync failed ({err}); the new generation is in place without proven durability",
                                    paths.active.display()
                                ),
                            },
                        };
                    }
                    Err(err) => {
                        format!("proven new generation at tmp but tmp → active failed: {err}")
                    }
                },
                Err(reason) => format!("tmp does not prove the new generation: {reason}"),
            };
            // Restore only a byte-exact prior generation.
            if prev_exists {
                match proves_old_generation(&paths.prev, generations.prior_bytes) {
                    Ok(()) => {
                        return match std::fs::rename(&paths.prev, &paths.active) {
                            Ok(()) => match sync_repair(parent) {
                                Ok(()) => SwapReconciliation::RestoredOld {
                                    detail: format!(
                                        "restored prior generation {} → {} ({temp_disposition})",
                                        paths.prev.display(),
                                        paths.active.display()
                                    ),
                                },
                                Err(err) => SwapReconciliation::Unreconciled {
                                    detail: format!(
                                        "restored prev → {} but the directory sync failed ({err}); the prior generation is in place without proven durability",
                                        paths.active.display()
                                    ),
                                },
                            },
                            Err(err) => SwapReconciliation::Unreconciled {
                                detail: format!(
                                    "no active rollout at {}; restoring the proven prior generation failed: {err} ({temp_disposition})",
                                    paths.active.display()
                                ),
                            },
                        };
                    }
                    Err(reason) => {
                        temp_disposition.push_str(&format!(
                            "; prev does not prove the prior generation: {reason}"
                        ));
                    }
                }
            } else {
                temp_disposition.push_str("; no prev");
            }
            SwapReconciliation::Unreconciled {
                detail: format!(
                    "no active rollout at {} ({temp_disposition})",
                    paths.active.display()
                ),
            }
        }
    }
}

// R12 (CX-S2): there is no rollback after a failed recorder reopen. The new
// generation was written and fsynced; restoring the oversized prior rollout
// would discard a completed compact. A failed reopen is recorded as a
// write-behind receipt (see `rollout_reconcile::RolloutReopenFailureReceipt`)
// and reconciled at the next open.

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
            bands = replacement_history.as_ref().map(|history| {
                history
                    .iter()
                    .map(|item| item.item.clone())
                    .collect::<Vec<_>>()
            });
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
                tail.push(response_item.item.clone());
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
        match serde_json::from_value::<RolloutLine>(value) {
            Ok(rollout_line) => items.push(rollout_line.item),
            Err(_) => continue,
        }
    }
    Ok(items)
}

/// RAII arm for a thread-scoped swap failpoint. Restores the previous value.
#[cfg(any(test, feature = "test-util"))]
pub struct SwapFailpointGuard {
    previous: u8,
}

#[cfg(any(test, feature = "test-util"))]
impl SwapFailpointGuard {
    pub fn arm(point: SwapFailpoint) -> Self {
        let previous = SWAP_FAILPOINT.with(|slot| slot.replace(point.bit()));
        Self { previous }
    }

    /// Arm several points at once (e.g. a swap stage plus a reconciliation
    /// repair failure).
    pub fn arm_all(points: &[SwapFailpoint]) -> Self {
        let mask = points.iter().fold(0u8, |mask, point| mask | point.bit());
        let previous = SWAP_FAILPOINT.with(|slot| slot.replace(mask));
        Self { previous }
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Drop for SwapFailpointGuard {
    fn drop(&mut self) {
        SWAP_FAILPOINT.with(|slot| slot.set(self.previous));
    }
}

#[cfg(any(test, feature = "test-util"))]
pub fn set_swap_failpoint(point: SwapFailpoint) {
    SWAP_FAILPOINT.with(|slot| slot.set(point.bit()));
}

#[cfg(any(test, feature = "test-util"))]
pub fn clear_swap_failpoint() {
    SWAP_FAILPOINT.with(|slot| slot.set(0));
}

fn maybe_fail(point: SwapFailpoint) -> std::io::Result<()> {
    #[cfg(any(test, feature = "test-util"))]
    {
        let mask = SWAP_FAILPOINT.with(std::cell::Cell::get);
        if point != SwapFailpoint::None && mask & point.bit() != 0 {
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
    let mut ordinal_state = ordinal_state_for_items(items)?;
    let rollout_generation_id = Uuid::new_v4().to_string();
    for item in items {
        let ordinal = ordinal_state.current()?;
        let generation_id =
            matches!(item, RolloutItem::SessionMeta(_)).then_some(rollout_generation_id.as_str());
        write_one_line(&mut file, item, ordinal, generation_id)?;
        ordinal_state.advance();
    }
    file.flush()?;
    Ok(())
}

/// The writer's ordinal plan for a full rewrite of `items`: from the first
/// `SessionMeta`'s history mode / base / subagent start, legacy when there
/// is none. Shared with the new-generation proof so both derive the same
/// envelope.
fn ordinal_state_for_items(items: &[RolloutItem]) -> std::io::Result<RolloutOrdinalState> {
    items
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(&meta.meta),
            _ => None,
        })
        .map_or_else(
            || Ok(RolloutOrdinalState::Legacy),
            |meta| {
                RolloutOrdinalState::for_rewrite(
                    meta.history_mode,
                    meta.history_base,
                    meta.subagent_history_start_ordinal,
                    items.len(),
                )
            },
        )
}

fn write_one_line(
    file: &mut File,
    item: &RolloutItem,
    ordinal: Option<u64>,
    rollout_generation_id: Option<&str>,
) -> std::io::Result<()> {
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

    #[derive(Serialize)]
    struct Line<'a> {
        timestamp: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ordinal: Option<u64>,
        #[serde(
            rename = "rollout_generation_id",
            skip_serializing_if = "Option::is_none"
        )]
        rollout_generation_id: Option<&'a str>,
        #[serde(flatten)]
        item: &'a RolloutItem,
    }

    let mut json = serde_json::to_string(&Line {
        timestamp,
        ordinal,
        rollout_generation_id,
        item,
    })
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
        // std::fs::File cannot open a directory on native Windows. Directory
        // fsync is explicitly best-effort here; the file was already flushed.
        Err(err) if cfg!(windows) && err.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[path = "rollout_swap_tests.rs"]
mod tests;
