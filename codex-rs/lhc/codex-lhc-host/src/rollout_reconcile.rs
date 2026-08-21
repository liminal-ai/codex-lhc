//! Startup rollout reconciliation (slice E).
//!
//! LHC's SQLite is the source of truth; the rollout is a regenerable projection.
//! At session open, classify the file against the thread and regenerate via
//! materialize + atomic swap when MISSING / CORRUPT / STALE.
//!
//! Fail-open: if the LHC thread is unavailable, leave the file alone (native
//! behavior). Loud `info!` logs name which state triggered a rewrite.

use std::path::Path;
use std::path::PathBuf;

use codex_history::CompactedItem;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tracing::info;
use tracing::warn;

use codex_protocol::models::ResponseItem;

use crate::compact_bridge::CompactMarker;
use crate::compact_bridge::read_materialize_surfaces;
use crate::inference::lhc_inference_callbacks;
use crate::materialize::CompactBoundaryMeta;
use crate::materialize::MaterializeInput;
use crate::materialize::materialize_rollout;
use crate::rollout_swap::atomic_rewrite_rollout;
use crate::rollout_swap::parse_rollout_items;
use crate::session::LhcSession;

/// Why a startup rewrite was (or would be) triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutReconcileTrigger {
    /// Rollout path does not exist on disk.
    Missing,
    /// File exists but is unparseable / has no usable structure.
    Corrupt,
    /// LHC compact point advanced past the file's newest Compacted boundary
    /// (crash window between LHC commit and rename).
    Stale,
    /// R12/G26 (CX-S4): a reopen-failure receipt proved canonical suffix
    /// events beyond the compacted rollout's frontier, and they were replayed
    /// onto it. Never produced by [`classify_rollout_vs_thread`].
    ReopenSuffixReplay,
    /// R12/G26 (CX-S4): a reopen failure is known to have happened but its
    /// accounting is unusable, so the rollout is rebuilt from the best
    /// available LHC view. Never produced by [`classify_rollout_vs_thread`].
    ReopenAccountingUnavailable,
}

/// Classification of a rollout file relative to an LHC thread compact point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutFileClass {
    /// File matches the thread (or both are pre-compact).
    Ok,
    /// Rewrite required for the named reason.
    NeedsRewrite(RolloutReconcileTrigger),
}

/// True when the rollout carries more than one `Compacted` record — the
/// native-append-polluted shape after an LHC rewrite plus a later native append.
pub fn is_native_append_polluted(items: &[RolloutItem]) -> bool {
    compacted_record_count(items) > 1
}

/// Count of `Compacted` records in a rollout item sequence.
pub fn compacted_record_count(items: &[RolloutItem]) -> usize {
    items
        .iter()
        .filter(|item| matches!(item, RolloutItem::Compacted(_)))
        .count()
}

/// Compact point embedded in the newest `Compacted` durable / marker message.
///
/// Scans newest-first. Accepts `lhc_compact_durable` full markers and the
/// summary-shaped `lhc_compact_marker` notes that carry `compactPoint`.
pub fn file_boundary_compact_point(items: &[RolloutItem]) -> Option<i64> {
    for item in items.iter().rev() {
        let RolloutItem::Compacted(CompactedItem { message, .. }) = item else {
            continue;
        };
        if let Some(point) = compact_point_from_boundary_message(message) {
            return Some(point);
        }
    }
    None
}

fn compact_point_from_boundary_message(message: &str) -> Option<i64> {
    if let Some(marker) = CompactMarker::parse_durable_writeback_record(message) {
        return Some(marker.compact_point);
    }
    let json = message
        .strip_prefix("lhc_compact_marker ")
        .or_else(|| {
            message
                .find("lhc_compact_marker ")
                .map(|idx| message[idx + "lhc_compact_marker ".len()..].trim())
        })
        .unwrap_or(message);
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value
        .get("compactPoint")
        .and_then(serde_json::Value::as_i64)
        .or_else(|| {
            value
                .get("compact_point")
                .and_then(serde_json::Value::as_i64)
        })
}

/// Classify a rollout path against the thread's latest compact point.
///
/// `lhc_compact_point` is `None` when the thread itself is unavailable — the
/// caller must fail-open (do not rewrite). When present, `0` means the thread
/// has not compacted yet.
pub fn classify_rollout_vs_thread(
    path: &Path,
    lhc_compact_point: Option<i64>,
) -> Result<RolloutFileClass, RolloutReconcileTrigger> {
    // Thread unavailable is not a classify outcome — caller fail-opens.
    let Some(lhc_point) = lhc_compact_point else {
        return Ok(RolloutFileClass::Ok);
    };

    if !path.exists() {
        return Ok(RolloutFileClass::NeedsRewrite(
            RolloutReconcileTrigger::Missing,
        ));
    }

    let meta = std::fs::metadata(path).map_err(|_| RolloutReconcileTrigger::Corrupt)?;
    if !meta.is_file() {
        return Ok(RolloutFileClass::NeedsRewrite(
            RolloutReconcileTrigger::Corrupt,
        ));
    }

    let items = match parse_rollout_items(path) {
        Ok(items) => items,
        Err(_) => {
            return Ok(RolloutFileClass::NeedsRewrite(
                RolloutReconcileTrigger::Corrupt,
            ));
        }
    };

    let has_session_meta = items
        .iter()
        .any(|item| matches!(item, RolloutItem::SessionMeta(_)));
    if items.is_empty() || !has_session_meta {
        // Non-empty garbage that parses to zero usable lines, or empty file.
        if meta.len() > 0 || items.is_empty() {
            return Ok(RolloutFileClass::NeedsRewrite(
                RolloutReconcileTrigger::Corrupt,
            ));
        }
    }

    let file_point = file_boundary_compact_point(&items).unwrap_or(0);
    if lhc_point > file_point {
        return Ok(RolloutFileClass::NeedsRewrite(
            RolloutReconcileTrigger::Stale,
        ));
    }

    Ok(RolloutFileClass::Ok)
}

/// Outcome of a reconcile attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// File was left alone (ok, or thread unavailable fail-open).
    Unchanged { reason: &'static str },
    /// File was regenerated from the LHC thread.
    Regenerated {
        trigger: RolloutReconcileTrigger,
        items: usize,
    },
}

/// R12 (CX-S2): filename suffix of the reopen-failure receipt sidecar.
pub const ROLLOUT_REOPEN_RECEIPT_SUFFIX: &str = ".reopen-failure.json";

/// Sidecar path holding the reopen-failure receipt for `rollout_path`.
pub fn rollout_reopen_receipt_path(rollout_path: &Path) -> PathBuf {
    let mut receipt = rollout_path.as_os_str().to_os_string();
    receipt.push(ROLLOUT_REOPEN_RECEIPT_SUFFIX);
    PathBuf::from(receipt)
}

/// Canonical LHC capture frontier at a point in time: the archive's newest
/// event order plus the number of captured events behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureFrontier {
    /// Newest `event_order` in the LHC archive (canonical event ordering).
    pub last_event_order: i64,
    /// Number of captured events at that frontier.
    pub event_count: u64,
}

/// Identity of a compacted rollout generation: content hash plus size, so a
/// later open can tell whether the file it finds is the one the receipt
/// describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactedRolloutIdentity {
    /// Hex sha256 over the rewritten rollout file's bytes.
    pub sha256: String,
    pub bytes: u64,
    /// Parseable rollout records in the file.
    pub items: u64,
}

/// R12 (CX-S2): durable record of a rollout whose append recorder could not
/// reopen onto the newly installed inode.
///
/// The compacted rollout was fsynced and stays authoritative — this receipt is
/// **write-behind diagnostics**, never authority. It exists so the next open
/// (CX-S4) can compare the compacted-rollout frontier against the canonical
/// LHC capture frontier and replay only the suffix that is provably present in
/// LHC beyond the rollout. Nothing here may veto a compact, and a receipt that
/// cannot be written only costs the next open its accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutReopenFailureReceipt {
    /// Schema tag so a future shape change is detectable, not misread.
    pub schema: String,
    pub written_at: String,
    pub thread_id: String,
    pub rollout_path: String,
    /// Compacted rollout the recorder failed to reopen onto.
    pub compacted_rollout: CompactedRolloutIdentity,
    /// Recorder append frontier at failure: records durably in the compacted
    /// rollout when appends stopped landing in it.
    pub recorder_frontier_items: u64,
    /// Canonical LHC capture frontier at failure (`None` when the archive
    /// could not be read — the receipt is still worth writing).
    pub capture_frontier: Option<CaptureFrontier>,
    /// Reopen error text, for operator diagnosis only.
    pub reopen_error: String,
}

/// Current schema tag written into [`RolloutReopenFailureReceipt::schema`].
pub const ROLLOUT_REOPEN_RECEIPT_SCHEMA: &str = "lhc.rollout_reopen_failure.v1";

/// Hash + measure a rewritten rollout file for [`CompactedRolloutIdentity`].
pub fn compacted_rollout_identity(
    rollout_path: &Path,
) -> std::io::Result<CompactedRolloutIdentity> {
    let bytes = std::fs::read(rollout_path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let items = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .count() as u64;
    Ok(CompactedRolloutIdentity {
        sha256: format!("{:x}", hasher.finalize()),
        bytes: bytes.len() as u64,
        items,
    })
}

/// Read the canonical LHC capture frontier (newest event order + count).
///
/// Returns `None` when the archive is unavailable — the caller records the
/// receipt without it rather than dropping the receipt.
pub async fn read_capture_frontier(
    thread_id: &str,
    root: Option<&Path>,
) -> Option<CaptureFrontier> {
    let root_buf = root
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::gating::lhc_root);
    if !crate::session::thread_file_path(&root_buf, thread_id).exists() {
        return None;
    }
    let callbacks = lhc_inference_callbacks(false).ok()?;
    let (session, _) =
        LhcSession::open_with_inference(thread_id, None, Some(root_buf.as_path()), callbacks)
            .await?;
    let events = session.list_events().await.ok();
    session.close().await;
    let events = events?;
    Some(CaptureFrontier {
        last_event_order: events
            .iter()
            .map(lhc::intake_stream::EventRecord::event_order)
            .max()
            .unwrap_or(0),
        event_count: events.len() as u64,
    })
}

/// Persist a reopen-failure receipt beside `rollout_path`.
///
/// Write-behind: the caller warns on failure and keeps the compacted rollout.
pub fn write_rollout_reopen_failure_receipt(
    rollout_path: &Path,
    receipt: &RolloutReopenFailureReceipt,
) -> std::io::Result<()> {
    let path = rollout_reopen_receipt_path(rollout_path);
    let json = serde_json::to_vec_pretty(receipt)
        .map_err(|e| std::io::Error::other(format!("serialize reopen receipt: {e}")))?;
    std::fs::write(&path, json)
}

/// Read a reopen-failure receipt written beside `rollout_path`, if any.
///
/// Unreadable / unparseable receipts read as absent: the receipt informs the
/// next open, it never gates it.
pub fn read_rollout_reopen_failure_receipt(
    rollout_path: &Path,
) -> Option<RolloutReopenFailureReceipt> {
    let path = rollout_reopen_receipt_path(rollout_path);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// R12/G26 (CX-S4): what the next open did with a reopen-failure receipt.
///
/// The compacted rollout is authoritative in every arm. Nothing here can put
/// the oversized prior generation back — the receipt exists to name what was
/// lost or to replay what canonical LHC can still prove, never to roll back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReopenReceiptOutcome {
    /// No receipt beside this rollout — ordinary open.
    Absent,
    /// The receipt describes a rollout generation that is no longer on disk:
    /// a later rewrite already superseded it.
    Superseded { detail: String },
    /// Frontiers compared: canonical LHC holds nothing beyond the compacted
    /// rollout's frontier.
    NoSuffix,
    /// Suffix events provably present in canonical LHC beyond the rollout
    /// frontier were replayed onto the compacted rollout, in canonical order,
    /// exactly once.
    Replayed { appended: usize, total_items: usize },
    /// The receipt proves canonical events the archive can no longer produce.
    /// The compacted rollout stands and the loss is named explicitly.
    KnownGap { warning: String, events: u64 },
    /// A reopen failure is known to have happened, but its accounting is
    /// unusable (unreadable receipt, no recorded capture frontier, or a
    /// canonical materialization that does not overlap the rollout). The next
    /// open rebuilds from the best available LHC view.
    AccountingUnavailable { detail: String },
    /// The suffix was computed but could not be written. The receipt stays on
    /// disk so a later open can retry; the compacted rollout is untouched.
    ReplayFailed { detail: String },
}

/// Consume the receipt: it is diagnostics, and a consumed one must not make
/// the next open warn or replay a second time.
fn clear_reopen_failure_receipt(rollout_path: &Path) {
    let path = rollout_reopen_receipt_path(rollout_path);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(
            %err,
            path = %path.display(),
            "reopen-failure receipt could not be consumed; the next open re-reads it \
             (replay stays idempotent by item identity)"
        ),
    }
}

/// ResponseItems after the newest `Compacted` boundary — the rollout's active
/// tail, which is the only region a suffix replay may touch.
fn tail_after_last_boundary(items: &[RolloutItem]) -> Vec<&ResponseItem> {
    let last_boundary = items
        .iter()
        .rposition(|item| matches!(item, RolloutItem::Compacted(_)));
    items
        .iter()
        .skip(last_boundary.map_or(0, |i| i + 1))
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(ri) => Some(&ri.item),
            _ => None,
        })
        .collect()
}

/// R12/G26 (CX-S4): next-open consumer for the reopen-failure receipt CX-S2
/// persists after a compacted rollout's append recorder failed to reopen.
///
/// Compares three frontiers:
/// (a) the receipt's recorder frontier + canonical capture frontier at
///     failure, (b) the canonical LHC capture frontier/tail right now, and
/// (c) the frontier reconstructed from the compacted rollout on disk.
///
/// Replays **only** the suffix events provably present in canonical LHC
/// beyond the reconstructed rollout frontier, in canonical order, skipping
/// any item already present (idempotent: a second open appends nothing).
///
/// When the receipt proves a frontier the archive can no longer produce, the
/// session continues on the compacted rollout and the loss is named with an
/// exact bounded range and count. Recovery is never inferred from the fact
/// that the open succeeded, and the oversized prior rollout is never restored.
pub async fn consume_reopen_failure_receipt(
    path: &Path,
    thread_id: &str,
    root: Option<&Path>,
    live_identity: Option<crate::mapping::ModelIdentity>,
) -> ReopenReceiptOutcome {
    let receipt_path = rollout_reopen_receipt_path(path);
    if !receipt_path.exists() {
        return ReopenReceiptOutcome::Absent;
    }

    let Some(receipt) = read_rollout_reopen_failure_receipt(path) else {
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::AccountingUnavailable {
            detail: format!(
                "reopen-failure receipt at {} is unreadable or unparseable",
                receipt_path.display()
            ),
        };
    };
    if receipt.schema != ROLLOUT_REOPEN_RECEIPT_SCHEMA {
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::AccountingUnavailable {
            detail: format!(
                "reopen-failure receipt schema {:?} is not {ROLLOUT_REOPEN_RECEIPT_SCHEMA:?}; \
                 refusing to interpret unknown accounting",
                receipt.schema
            ),
        };
    }
    if receipt.thread_id != thread_id {
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::Superseded {
            detail: format!(
                "reopen-failure receipt names thread {} but this open is thread {thread_id}",
                receipt.thread_id
            ),
        };
    }

    // (c) Reconstruct the compacted-rollout frontier from the file on disk,
    // and prove it is the generation the receipt describes.
    let rollout_items = match parse_rollout_items(path) {
        Ok(items) => items,
        Err(err) => {
            clear_reopen_failure_receipt(path);
            return ReopenReceiptOutcome::AccountingUnavailable {
                detail: format!(
                    "compacted rollout {} is unreadable ({err}); no frontier to reconcile against",
                    path.display()
                ),
            };
        }
    };
    match compacted_rollout_identity(path) {
        Ok(identity) if identity == receipt.compacted_rollout => {}
        Ok(identity) => {
            clear_reopen_failure_receipt(path);
            return ReopenReceiptOutcome::Superseded {
                detail: format!(
                    "rollout on disk (sha256 {}, {} items) is not the receipt's compacted \
                     generation (sha256 {}, {} items)",
                    identity.sha256,
                    identity.items,
                    receipt.compacted_rollout.sha256,
                    receipt.compacted_rollout.items
                ),
            };
        }
        Err(err) => {
            clear_reopen_failure_receipt(path);
            return ReopenReceiptOutcome::AccountingUnavailable {
                detail: format!(
                    "compacted rollout identity for {} unreadable ({err})",
                    path.display()
                ),
            };
        }
    }

    // The recorder frontier the receipt recorded must agree with the
    // compacted generation it names. Where it does not, the receipt's
    // accounting is internally inconsistent — replaying on top of it would
    // be a guess, and guesses are exactly what this protocol exists to
    // prevent. (A zero frontier is the identity-unreadable fallback at write
    // time and carries no independent claim, so it is not checked.)
    if receipt.recorder_frontier_items != 0
        && receipt.recorder_frontier_items != receipt.compacted_rollout.items
    {
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::AccountingUnavailable {
            detail: format!(
                "reopen receipt's recorder frontier ({} items) disagrees with its own \
                 compacted generation ({} items); accounting is inconsistent, not replaying",
                receipt.recorder_frontier_items, receipt.compacted_rollout.items
            ),
        };
    }

    // (a) The receipt's canonical capture frontier at failure.
    let Some(receipt_capture) = receipt.capture_frontier else {
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::AccountingUnavailable {
            detail: format!(
                "reopen-failure receipt for {} records no canonical capture frontier \
                 (recorder frontier {} items); nothing to bound a replay with",
                path.display(),
                receipt.recorder_frontier_items
            ),
        };
    };

    // (b) The canonical capture frontier now.
    let Some(now) = read_capture_frontier(thread_id, root).await else {
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::KnownGap {
            warning: format!(
                "canonical LHC archive unavailable at next open: the reopen receipt proves \
                 {events} captured events through event_order {order}, and none of them can be \
                 replayed onto the compacted rollout ({path}); continuing on the compacted \
                 rollout (the oversized prior generation is never restored)",
                events = receipt_capture.event_count,
                order = receipt_capture.last_event_order,
                path = path.display()
            ),
            events: receipt_capture.event_count,
        };
    };

    if now.last_event_order < receipt_capture.last_event_order
        || now.event_count < receipt_capture.event_count
    {
        // Count what the receipt proved and the archive can no longer produce.
        // When the counts agree but the ordering regressed, fall back to the
        // width of the missing event_order range.
        let missing_events = receipt_capture.event_count.saturating_sub(now.event_count);
        let missing = if missing_events > 0 {
            missing_events
        } else {
            receipt_capture
                .last_event_order
                .saturating_sub(now.last_event_order)
                .max(0) as u64
        };
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::KnownGap {
            warning: format!(
                "reopen receipt proves canonical capture through event_order {r_order} \
                 ({r_count} events); the archive now ends at event_order {n_order} \
                 ({n_count} events): event_order range ({n_order}, {r_order}] is unavailable \
                 ({missing} events lost); continuing on the compacted rollout (the oversized \
                 prior generation is never restored)",
                r_order = receipt_capture.last_event_order,
                r_count = receipt_capture.event_count,
                n_order = now.last_event_order,
                n_count = now.event_count,
            ),
            events: missing,
        };
    }

    if now.last_event_order == receipt_capture.last_event_order
        && now.event_count == receipt_capture.event_count
    {
        // Open succeeding is not evidence of recovery — the frontiers are.
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::NoSuffix;
    }

    // Canonical capture advanced past the receipt frontier while appends were
    // going to the orphaned inode. Replay exactly that suffix.
    let canonical = match materialize_thread_rollout_items(
        path,
        thread_id,
        root,
        RolloutReconcileTrigger::ReopenSuffixReplay,
        live_identity,
    )
    .await
    {
        Ok(items) => items,
        Err(err) => {
            clear_reopen_failure_receipt(path);
            return ReopenReceiptOutcome::AccountingUnavailable {
                detail: format!("canonical materialization unavailable for replay: {err}"),
            };
        }
    };

    let rollout_tail = tail_after_last_boundary(&rollout_items);
    let canonical_tail = tail_after_last_boundary(&canonical);

    // Occurrence-aware ordered alignment. In this G26 geometry the compacted
    // rollout tail must be an exact ordered prefix of the current canonical
    // tail (both cut at the same newest Compacted boundary), compared
    // position-by-position on id-insensitive bytes. Repeated identical
    // content is legal — distinct events may serialize identically — so no
    // content-set membership is consulted anywhere: an any-position anchor
    // can seize a later duplicate and skip real suffix, and a global
    // contains-filter drops legitimately repeated suffix events. If the
    // rollout tail is not an exact ordered prefix, there is no provable
    // boundary and the answer is AccountingUnavailable, not a guess.
    let is_ordered_prefix = rollout_tail.len() <= canonical_tail.len()
        && rollout_tail
            .iter()
            .zip(canonical_tail.iter())
            .all(|(r, c)| {
                crate::body_validation::item_bytes_without_id(r)
                    == crate::body_validation::item_bytes_without_id(c)
            });
    if !is_ordered_prefix {
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::AccountingUnavailable {
            detail: format!(
                "compacted rollout tail ({} items) is not an ordered prefix of the canonical \
                 materialized tail ({} items); no provable suffix boundary, not replaying",
                rollout_tail.len(),
                canonical_tail.len()
            ),
        };
    }

    // Everything past the aligned prefix is provably beyond the rollout
    // frontier — replayed exactly, duplicates included.
    let suffix: Vec<RolloutItem> = canonical_tail[rollout_tail.len()..]
        .iter()
        .map(|item| RolloutItem::ResponseItem((*item).clone().into()))
        .collect();
    if suffix.is_empty() {
        clear_reopen_failure_receipt(path);
        return ReopenReceiptOutcome::NoSuffix;
    }

    let appended = suffix.len();
    let mut merged = rollout_items;
    merged.extend(suffix);
    if let Err(err) = atomic_rewrite_rollout(path, &merged) {
        // Keep the receipt: the replay is retryable and still idempotent.
        return ReopenReceiptOutcome::ReplayFailed {
            detail: format!("suffix replay rewrite {} failed: {err}", path.display()),
        };
    }
    clear_reopen_failure_receipt(path);
    ReopenReceiptOutcome::Replayed {
        appended,
        total_items: merged.len(),
    }
}

/// Map a consumed receipt onto the reconcile outcome, warning at the volume
/// each arm deserves.
async fn apply_reopen_failure_receipt(
    path: &Path,
    thread_id: &str,
    root: Option<&Path>,
    live_identity: Option<crate::mapping::ModelIdentity>,
) -> ReconcileOutcome {
    match consume_reopen_failure_receipt(path, thread_id, root, live_identity.clone()).await {
        ReopenReceiptOutcome::Absent | ReopenReceiptOutcome::NoSuffix => {
            ReconcileOutcome::Unchanged { reason: "ok" }
        }
        ReopenReceiptOutcome::Superseded { detail } => {
            info!(
                %detail,
                path = %path.display(),
                thread_id,
                "LHC startup reconciliation: reopen-failure receipt superseded; discarded"
            );
            ReconcileOutcome::Unchanged { reason: "ok" }
        }
        ReopenReceiptOutcome::Replayed {
            appended,
            total_items,
        } => {
            warn!(
                path = %path.display(),
                thread_id,
                appended,
                total_items,
                "LHC startup reconciliation: replayed the canonical suffix a dead recorder \
                 handle never appended onto the compacted rollout"
            );
            ReconcileOutcome::Regenerated {
                trigger: RolloutReconcileTrigger::ReopenSuffixReplay,
                items: total_items,
            }
        }
        ReopenReceiptOutcome::KnownGap { warning, events } => {
            warn!(
                %warning,
                path = %path.display(),
                thread_id,
                events,
                "LHC startup reconciliation: EXPLICIT LOSS — canonical payload unavailable for a \
                 range the reopen receipt proved; the compacted rollout stands"
            );
            ReconcileOutcome::Unchanged {
                reason: "reopen_gap_unrecoverable",
            }
        }
        ReopenReceiptOutcome::ReplayFailed { detail } => {
            warn!(
                %detail,
                path = %path.display(),
                thread_id,
                "LHC startup reconciliation: suffix replay could not be written; receipt kept \
                 for a later open; compacted rollout untouched"
            );
            ReconcileOutcome::Unchanged {
                reason: "reopen_replay_failed",
            }
        }
        ReopenReceiptOutcome::AccountingUnavailable { detail } => {
            warn!(
                %detail,
                path = %path.display(),
                thread_id,
                "LHC startup reconciliation: reopen accounting unavailable; rebuilding the \
                 rollout from the best available LHC view"
            );
            let trigger = RolloutReconcileTrigger::ReopenAccountingUnavailable;
            match regenerate_rollout_from_thread(path, thread_id, root, trigger, live_identity)
                .await
            {
                Ok(items) => {
                    info!(
                        path = %path.display(),
                        thread_id,
                        items,
                        "LHC startup reconciliation: rollout rebuilt from LHC view without \
                         reopen accounting"
                    );
                    ReconcileOutcome::Regenerated { trigger, items }
                }
                Err(err) => {
                    warn!(
                        %err,
                        path = %path.display(),
                        thread_id,
                        "LHC startup reconciliation: rebuild without accounting failed; \
                         compacted rollout stands"
                    );
                    ReconcileOutcome::Unchanged {
                        reason: "regenerate_failed",
                    }
                }
            }
        }
    }
}

/// Read the thread's latest compact point from the archive (fail-open → `None`).
///
/// Does **not** create a new thread: missing thread file means unavailable.
pub async fn read_thread_compact_point(thread_id: &str, root: Option<&Path>) -> Option<i64> {
    let root_buf = root
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::gating::lhc_root);
    let file_path = crate::session::thread_file_path(&root_buf, thread_id);
    if !file_path.exists() {
        return None;
    }

    // nc4: read the installed view's compact_point directly from the
    // thread_view table via SDK describe(). This is the authoritative
    // source — it advances atomically when LHC compact installs a view,
    // even if the Codex marker note was not yet committed to events
    // (crash window between SDK compact and host marker write-back).
    let ref_ = lhc::threads::ThreadRef::file_path(file_path.to_string_lossy().into_owned());
    match lhc::thread_view::describe(ref_).await {
        lhc::shared_tech::errors::OpResult::Ok { value: Some(view) } => Some(view.compact_point),
        lhc::shared_tech::errors::OpResult::Ok { value: None } => {
            // Thread exists but has no view yet — pre-compact.
            Some(0)
        }
        lhc::shared_tech::errors::OpResult::Err { error } => {
            warn!(
                thread_id,
                reason = %error.reason,
                "LHC reconcile: describe failed; falling back to event scan"
            );
            // Fall back to the event-scan path for robustness.
            let callbacks = lhc_inference_callbacks(false).ok()?;
            let (session, _) = LhcSession::open_with_inference(
                thread_id,
                None,
                Some(root_buf.as_path()),
                callbacks,
            )
            .await?;
            let events = match session.list_events().await {
                Ok(e) => e,
                Err(err) => {
                    warn!(%err, thread_id, "LHC reconcile: list_events failed; fail-open");
                    session.close().await;
                    return None;
                }
            };
            let point = latest_compact_point_from_events(&events);
            session.close().await;
            point
        }
    }
}

/// Best-effort max compact point from archive event notes.
pub fn latest_compact_point_from_events(events: &[lhc::intake_stream::EventRecord]) -> Option<i64> {
    let mut best: Option<i64> = None;
    for ev in events {
        let Some(tp) = ev.text_payload() else {
            continue;
        };
        if let Some(point) = compact_point_from_boundary_message(&tp.text) {
            best = Some(best.map_or(point, |b| b.max(point)));
        }
    }
    // Presence of any marker note with compactPoint 0 still means "known".
    // Absence of markers → treat as pre-compact (Some(0) only when we saw a
    // thread open successfully with zero markers — callers pass Some(0)).
    best.or(Some(0))
}

/// R11 (CX-S6): unresolved host-validation warning descriptor.
///
/// When the newest compact-continuation receipt records an installed view
/// whose full-body host validation is `awaiting` or `failed` (and the durable
/// host-validation row has not since been recorded `ok`), this returns a
/// description of that unresolved state — for a loud warning, never for a
/// veto. The validation ACK is bookkeeping: its absence must not keep the
/// prior (typically oversized) rollout generation authoritative on the next
/// open. Regeneration proceeds from the installed LHC view through the
/// ordinary degrade path; a rerun validation failure degrades per R10 with no
/// backward fallback. Returns `None` when the state is resolved or superseded.
pub async fn host_validation_reload_warning(
    thread_id: &str,
    root: Option<&Path>,
) -> Option<String> {
    let root_buf = root
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::gating::lhc_root);
    let file_path = crate::session::thread_file_path(&root_buf, thread_id);
    if !file_path.exists() {
        return None;
    }
    let ref_ = lhc::threads::ThreadRef::file_path(file_path.to_string_lossy().into_owned());
    let receipts =
        match lhc::compact_continuation::list_compact_continuation_receipts(ref_.clone(), Some(1))
            .await
        {
            lhc::shared_tech::errors::OpResult::Ok { value } => value,
            lhc::shared_tech::errors::OpResult::Err { error } => {
                // Inspection failure is itself only worth a warning; it never
                // decides whether regeneration proceeds.
                return Some(format!(
                    "host-validation inspect failed ({}: {})",
                    error.code.as_str(),
                    error.reason
                ));
            }
        };
    let Some(latest) = receipts.first() else {
        return None;
    };
    let status = latest.receipt.residual.host_validation_status;
    let installed_pending = !latest.receipt.residual.prior_serving_view_intact
        && matches!(
            status,
            lhc::shared_tech::compact_continuation::HostValidationStatusFact::Awaiting
                | lhc::shared_tech::compact_continuation::HostValidationStatusFact::Failed
        );
    if !installed_pending {
        return None;
    }
    // LIM-69: the block is bound to the view this attempt installed. A later
    // successful compact that replaced the active view supersedes the old
    // Awaiting/Failed residual. Do not rewrite that residual as ok.
    match host_validation_attempt_view_superseded(&ref_, &latest.attempt_id).await {
        Ok(Some((attempt_view, active_view))) => {
            info!(
                attempt_id = %latest.attempt_id,
                %attempt_view,
                %active_view,
                "host-validation reload gate superseded: failed/awaiting view is no longer active"
            );
            return None;
        }
        Ok(None) => {}
        Err(err) => {
            return Some(format!(
                "host-validation view-scope inspect failed for attempt {} ({})",
                latest.attempt_id, err
            ));
        }
    }
    // The durable host-validation row may have been resolved `ok` after the
    // receipt was recorded (validated later in the same or a prior process).
    match lhc::compact_continuation::get_compact_continuation_host_validation(
        ref_,
        &latest.attempt_id,
    )
    .await
    {
        lhc::shared_tech::errors::OpResult::Ok { value: Some(row) }
            if row.status == lhc::compact_continuation::HostValidationStatus::Ok =>
        {
            None
        }
        lhc::shared_tech::errors::OpResult::Ok { value: row } => Some(format!(
            "attempt {} installed a view whose host validation is {} (durable row: {:?})",
            latest.attempt_id,
            match status {
                lhc::shared_tech::compact_continuation::HostValidationStatusFact::Failed =>
                    "failed",
                _ => "awaiting",
            },
            row.map(|r| r.status),
        )),
        lhc::shared_tech::errors::OpResult::Err { error } => Some(format!(
            "host-validation row inspect failed for attempt {} ({}: {}); refusing regeneration",
            latest.attempt_id,
            error.code.as_str(),
            error.reason
        )),
    }
}

/// Returns `Some((attempt_view, active_view))` when the attempt's installed
/// view is no longer the thread's active serving view.
async fn host_validation_attempt_view_superseded(
    ref_: &lhc::threads::ThreadRef,
    attempt_id: &str,
) -> Result<Option<(String, String)>, String> {
    let Some(attempt_view) = installed_view_id_for_attempt(ref_, attempt_id).await? else {
        // No install_succeeded viewId: cannot prove supersession; keep the block.
        return Ok(None);
    };
    let active_view = match lhc::thread_view::describe(ref_.clone()).await {
        lhc::shared_tech::errors::OpResult::Ok { value: Some(view) } => view.view_id,
        lhc::shared_tech::errors::OpResult::Ok { value: None } => return Ok(None),
        lhc::shared_tech::errors::OpResult::Err { error } => {
            return Err(format!("{}: {}", error.code.as_str(), error.reason));
        }
    };
    if active_view != attempt_view {
        Ok(Some((attempt_view, active_view)))
    } else {
        Ok(None)
    }
}

async fn installed_view_id_for_attempt(
    ref_: &lhc::threads::ThreadRef,
    attempt_id: &str,
) -> Result<Option<String>, String> {
    let stages =
        match lhc::compact_continuation::list_compact_continuation_stages(ref_.clone(), attempt_id)
            .await
        {
            lhc::shared_tech::errors::OpResult::Ok { value } => value,
            lhc::shared_tech::errors::OpResult::Err { error } => {
                return Err(format!("{}: {}", error.code.as_str(), error.reason));
            }
        };
    let view_id = stages.into_iter().rev().find_map(|entry| {
        if entry.stage != "install_succeeded" {
            return None;
        }
        entry
            .detail
            .as_ref()
            .and_then(|d| d.get("viewId"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    Ok(view_id)
}

/// Classify + regenerate when needed. Fail-open if the thread is unavailable.
///
/// Loud `info!` names the trigger state on every rewrite.
pub async fn reconcile_rollout_at_path(
    path: &Path,
    thread_id: &str,
    root: Option<&Path>,
    live_identity: Option<crate::mapping::ModelIdentity>,
) -> ReconcileOutcome {
    let lhc_point = read_thread_compact_point(thread_id, root).await;
    if lhc_point.is_none() {
        return ReconcileOutcome::Unchanged {
            reason: "thread_unavailable",
        };
    }

    let class = match classify_rollout_vs_thread(path, lhc_point) {
        Ok(c) => c,
        Err(trigger) => RolloutFileClass::NeedsRewrite(trigger),
    };

    let RolloutFileClass::NeedsRewrite(trigger) = class else {
        // R12/G26 (CX-S4): the file matches the thread's compact point, but a
        // reopen-failure receipt beside it means appends stopped landing in
        // this inode while canonical capture kept advancing. A clean
        // classification is not evidence of recovery — compare the frontiers.
        // The probe costs one `exists()` on every ordinary open.
        if !rollout_reopen_receipt_path(path).exists() {
            return ReconcileOutcome::Unchanged { reason: "ok" };
        }
        // R11 (CX-S6): unresolved host validation is a warning, never a veto.
        // Receipt consumption proceeds; diagnostics record, they do not govern.
        if let Some(warning) = host_validation_reload_warning(thread_id, root).await {
            warn!(
                path = %path.display(),
                thread_id,
                %warning,
                "LHC startup reconciliation: consuming reopen receipt with unresolved \
                 host validation (warn-and-continue; the ACK is bookkeeping)"
            );
        }
        return apply_reopen_failure_receipt(path, thread_id, root, live_identity).await;
    };

    // R11 (CX-S6): an installed view with unresolved host validation still
    // regenerates. Keeping the prior (typically oversized) generation
    // authoritative because an ACK receipt is missing is the backward-fallback
    // gate this campaign removes — the missed next-open occurrence of G25. The
    // installed LHC view is what the session was serving; regeneration goes
    // through the ordinary degrade path and a rerun validation failure
    // degrades per R10. Warn loudly; never govern.
    if let Some(warning) = host_validation_reload_warning(thread_id, root).await {
        warn!(
            path = %path.display(),
            thread_id,
            ?trigger,
            %warning,
            "LHC startup reconciliation: regenerating from the installed view with \
             unresolved host validation (warn-and-continue; the ACK is bookkeeping)"
        );
    }

    match regenerate_rollout_from_thread(path, thread_id, root, trigger, live_identity).await {
        Ok(items) => {
            // A full rebuild from canonical LHC covers everything a reopen
            // receipt could have named, so the receipt is spent.
            if rollout_reopen_receipt_path(path).exists() {
                info!(
                    path = %path.display(),
                    thread_id,
                    "LHC startup reconciliation: reopen-failure receipt superseded by full \
                     regeneration from canonical LHC"
                );
                clear_reopen_failure_receipt(path);
            }
            info!(
                path = %path.display(),
                thread_id,
                ?trigger,
                items,
                "LHC startup reconciliation: regenerated rollout from thread"
            );
            ReconcileOutcome::Regenerated { trigger, items }
        }
        Err(err) => {
            warn!(
                %err,
                path = %path.display(),
                thread_id,
                ?trigger,
                "LHC startup reconciliation rewrite failed; leaving file alone (fail-open)"
            );
            ReconcileOutcome::Unchanged {
                reason: "regenerate_failed",
            }
        }
    }
}

/// Materialize the thread view and atomically swap it into `path`.
pub async fn regenerate_rollout_from_thread(
    path: &Path,
    thread_id: &str,
    root: Option<&Path>,
    trigger: RolloutReconcileTrigger,
    live_identity: Option<crate::mapping::ModelIdentity>,
) -> Result<usize, String> {
    let items =
        materialize_thread_rollout_items(path, thread_id, root, trigger, live_identity).await?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create rollout parent {}: {e}", parent.display()))?;
    }
    atomic_rewrite_rollout(path, &items)
        .map_err(|e| format!("atomic rewrite {}: {e}", path.display()))?;
    Ok(items.len())
}

/// Materialize the LHC thread into the rollout item sequence `path` should
/// hold, **without writing anything**.
///
/// Shared by full regeneration and by the R12/G26 suffix replay, which needs
/// the canonical sequence in order to diff it against the compacted rollout
/// already on disk.
pub async fn materialize_thread_rollout_items(
    path: &Path,
    thread_id: &str,
    root: Option<&Path>,
    trigger: RolloutReconcileTrigger,
    live_identity: Option<crate::mapping::ModelIdentity>,
) -> Result<Vec<RolloutItem>, String> {
    let root_buf = root
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::gating::lhc_root);
    let file_path = crate::session::thread_file_path(&root_buf, thread_id);
    if !file_path.exists() {
        return Err(format!(
            "thread unavailable (no archive at {})",
            file_path.display()
        ));
    }
    let surfaces = read_materialize_surfaces(thread_id, Some(root_buf.as_path()))
        .await
        .map_err(|e| format!("materialize surfaces: {e}"))?;

    // nc4: read the installed view metadata for boundary synthesis when no
    // Codex event marker exists (crash window between SDK compact and host
    // marker write-back).
    let ref_ = lhc::threads::ThreadRef::file_path(file_path.to_string_lossy().into_owned());
    let installed_view = match lhc::thread_view::describe(ref_).await {
        lhc::shared_tech::errors::OpResult::Ok { value } => value,
        lhc::shared_tech::errors::OpResult::Err { error } => {
            warn!(
                thread_id,
                reason = %error.reason,
                "nc4: describe failed during regeneration; boundary may be stale"
            );
            None
        }
    };

    let prior_generation = if path.exists() {
        parse_rollout_items(path).unwrap_or_else(|err| {
            warn!(
                %err,
                path = %path.display(),
                "reconcile: prior generation unparseable; carry-forwards empty"
            );
            Vec::new()
        })
    } else {
        Vec::new()
    };

    let session_meta = prior_generation
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(meta.clone()),
            _ => None,
        })
        .unwrap_or_else(|| synthesize_session_meta(thread_id, path));

    let (window_number, first_window_id, previous_window_id, window_id) =
        window_meta_from_prior(&prior_generation);

    let durable_message = latest_durable_marker_message(thread_id, Some(root_buf.as_path()))
        .await
        .unwrap_or_else(|| {
            // nc4: when no Codex event marker exists (crash between SDK
            // compact and host marker write-back), synthesize boundary
            // metadata from the installed view. Never stamp compactPoint=0
            // when the view is ahead.
            let (cp, cf, vid, prof) = match installed_view.as_ref() {
                Some(v) => (
                    v.compact_point,
                    v.covered_from,
                    v.view_id.as_str(),
                    v.profile_name.as_deref(),
                ),
                None => (0, 0, "reconcile", None),
            };
            format!(
                "lhc_compact_durable {}",
                serde_json::json!({
                    "viewId": vid,
                    "coveredFrom": cf,
                    "compactPoint": cp,
                    "totalTokens": 0,
                    "tailTokens": 0,
                    "firstKeptMessageId": null,
                    "profile": prof,
                    "bands": null,
                    "viewMapSeam": crate::compact_bridge::VIEW_MAP_SEAM_ID,
                    "bodyItemCount": 0,
                    "markerKey": format!("codex:{thread_id}:compact_marker:reconcile:{cp}"),
                    "derivedContentDigests": [],
                    "derivedHostIds": [],
                    "archiveTip": "reconcile",
                })
            )
        });

    let mut result = materialize_rollout(&MaterializeInput {
        session_meta,
        thread_view: &surfaces.thread_view,
        messages: &surfaces.messages,
        turns: &surfaces.turns,
        prior_generation: &prior_generation,
        boundary: CompactBoundaryMeta {
            message: durable_message,
            window_number,
            first_window_id,
            previous_window_id,
            window_id,
        },
        world_state: None,
        turn_context: None,
        live_identity,
    });

    for note in &result.gap_notes {
        warn!(%note, ?trigger, "LHC reconcile materialize gap_note");
    }

    // nc4: graft the exact provider-native active suffix from the prior
    // rollout into the regenerated materialization. The LHC round-trip
    // flattens CustomToolCall status/namespace and ContentItems; the prior
    // rollout holds the exact provider-native bytes. Graft per call_id, and
    // only when that call_id correlates unambiguously on both sides.
    //
    // R20 (CX-S4): **this intentionally supersedes the nc4-negotiated
    // preserve-prior-bytes behavior.** nc4 returned `Err` from the graft on
    // ambiguous / orphaned / missing correlation and left the prior rollout
    // byte-for-byte intact. That preserved a rollout which is both stale AND
    // oversized — the exact hazard startup reconciliation exists to clear —
    // in order to protect provider-specific decoration (status, namespace,
    // structured ContentItems) that the LHC-reconstructed pair does not need
    // in order to correlate: it carries the same call_id. R20 rules the other
    // way. Regeneration proceeds with the LHC-reconstructed pair, warns
    // loudly, and leaves the provider as the final authority on the degraded
    // body. A degraded-but-correlated body is recoverable; a session stranded
    // on a stale oversized rollout is not. Un-grafted call_ids keep their LHC
    // shape; unambiguous ones stay byte-exact.
    let graft = graft_prior_active_suffix(&mut result.items, &prior_generation);
    for reason in &graft.degraded {
        warn!(
            %reason,
            path = %path.display(),
            thread_id,
            ?trigger,
            "R20 (CX-S4): provider-native graft unavailable; regenerating with the \
             LHC-reconstructed pair (degraded body; the stale rollout is NOT preserved)"
        );
    }

    Ok(result.items)
}

/// Per-call_id result of the terminal-suffix graft.
///
/// R20 (CX-S4): the graft reports; it never vetoes. `degraded` names every
/// call_id whose provider-native pair could not be lifted across, and the
/// caller regenerates with the LHC-reconstructed pair for exactly those ids.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GraftReport {
    /// call_ids whose exact provider-native pair was grafted byte-for-byte.
    pub grafted: Vec<String>,
    /// One reason per call_id left on its LHC-reconstructed shape.
    pub degraded: Vec<String>,
}

/// Graft exact provider-native active tool call/output pairs from the prior
/// rollout's **terminal active suffix** into the regenerated materialization.
///
/// The terminal active suffix is the trailing sequence of tool calls/outputs
/// NOT followed by any assistant/Message response — i.e., the unsent provider
/// suffix that the next request still depends on.
///
/// R20 (CX-S4): ambiguous cardinality, an orphan output, or a call with no
/// output no longer aborts the regeneration. That call_id is reported in
/// [`GraftReport::degraded`] and keeps the LHC-reconstructed pair, which
/// carries the same call_id and correlation and differs only in provider
/// decoration. Every other call_id in the suffix is still grafted exactly.
pub(crate) fn graft_prior_active_suffix(
    regenerated: &mut [RolloutItem],
    prior_generation: &[RolloutItem],
) -> GraftReport {
    use crate::body_validation::client_call_id;
    use crate::body_validation::output_call_id;

    // Extract items after the last Compacted boundary.
    let last_boundary = prior_generation
        .iter()
        .rposition(|item| matches!(item, RolloutItem::Compacted(_)));
    let prior_tail: Vec<&ResponseItem> = prior_generation
        .iter()
        .skip(last_boundary.map_or(0, |i| i + 1))
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(ri) => Some(&ri.item),
            _ => None,
        })
        .collect();
    let mut report = GraftReport::default();
    if prior_tail.is_empty() {
        return report;
    }

    // Derive the terminal active suffix: scan backwards from the end;
    // stop at the first assistant/Message response (that marks completion).
    fn is_assistant_or_message(item: &ResponseItem) -> bool {
        matches!(
            item,
            ResponseItem::Message { role, .. } if role == "assistant"
        ) || matches!(item, ResponseItem::Compaction { .. })
    }
    let terminal_start = prior_tail
        .iter()
        .rposition(|item| is_assistant_or_message(item))
        .map_or(0, |i| i + 1);
    let terminal_suffix = &prior_tail[terminal_start..];
    if terminal_suffix.is_empty() {
        return report;
    }

    // Build the correlation-id set from BOTH client_call_id and output_call_id.
    // Every id present on either side must have exactly one call + one output.
    let mut seen_ids: Vec<String> = Vec::new();
    for item in terminal_suffix {
        if let Some(id) = client_call_id(item) {
            if !seen_ids.contains(&id) {
                seen_ids.push(id);
            }
        }
        if let Some(id) = output_call_id(item) {
            if !seen_ids.contains(&id) {
                seen_ids.push(id);
            }
        }
    }
    if seen_ids.is_empty() {
        return report;
    }
    // Every id needs exactly 1 call + 1 output on the prior side to be lifted
    // across byte-exactly. Anything else (1/0 missing output, 0/1 orphan
    // output, duplicates) is reported and left on the LHC pair — R20.
    let mut active_call_ids: Vec<String> = Vec::new();
    for id in &seen_ids {
        let call_count = terminal_suffix
            .iter()
            .filter(|i| client_call_id(i).as_deref() == Some(id.as_str()))
            .count();
        let output_count = terminal_suffix
            .iter()
            .filter(|i| output_call_id(i).as_deref() == Some(id.as_str()))
            .count();
        if call_count == 1 && output_count == 1 {
            active_call_ids.push(id.clone());
        } else {
            report.degraded.push(format!(
                "graft: missing or ambiguous correlation for call_id {id} \
                 in prior terminal suffix (calls={call_count}, outputs={output_count})"
            ));
        }
    }

    // Verify each active call_id has exactly 1 call + 1 output in the
    // regenerated items. If not, the correlation is ambiguous — fail.
    for call_id in &active_call_ids {
        let regen_calls: Vec<usize> = regenerated
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match item {
                RolloutItem::ResponseItem(ri)
                    if client_call_id(&ri.item).as_deref() == Some(call_id.as_str()) =>
                {
                    Some(i)
                }
                _ => None,
            })
            .collect();
        let regen_outputs: Vec<usize> = regenerated
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match item {
                RolloutItem::ResponseItem(ri)
                    if output_call_id(&ri.item).as_deref() == Some(call_id.as_str()) =>
                {
                    Some(i)
                }
                _ => None,
            })
            .collect();
        if regen_calls.len() != 1 || regen_outputs.len() != 1 {
            report.degraded.push(format!(
                "graft: ambiguous cardinality for call_id {call_id} in \
                 regenerated rollout (calls={}, outputs={})",
                regen_calls.len(),
                regen_outputs.len()
            ));
            continue;
        }

        // Graft the exact prior items.
        let Some(prior_call) = terminal_suffix
            .iter()
            .find(|item| client_call_id(item).as_deref() == Some(call_id.as_str()))
        else {
            report
                .degraded
                .push(format!("graft: prior call {call_id} vanished"));
            continue;
        };
        let Some(prior_output) = terminal_suffix
            .iter()
            .find(|item| output_call_id(item).as_deref() == Some(call_id.as_str()))
        else {
            report
                .degraded
                .push(format!("graft: prior output {call_id} vanished"));
            continue;
        };

        regenerated[regen_calls[0]] = RolloutItem::ResponseItem((*prior_call).clone().into());
        regenerated[regen_outputs[0]] = RolloutItem::ResponseItem((*prior_output).clone().into());
        report.grafted.push(call_id.clone());
    }
    report
}

fn synthesize_session_meta(thread_id: &str, path: &Path) -> SessionMetaLine {
    let id = ThreadId::from_string(thread_id)
        .ok()
        .or_else(|| thread_id_from_rollout_path(path))
        .unwrap_or_default();
    SessionMetaLine {
        meta: SessionMeta {
            id,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ..SessionMeta::default()
        },
        git: None,
    }
}

fn thread_id_from_rollout_path(path: &Path) -> Option<ThreadId> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl")?;
    // `rollout-<ts>-<uuid>.jsonl` — UUID is the trailing 36 characters.
    if stem.len() >= 36 {
        let candidate = &stem[stem.len() - 36..];
        if let Ok(id) = ThreadId::from_string(candidate) {
            return Some(id);
        }
    }
    None
}

fn window_meta_from_prior(prior: &[RolloutItem]) -> (u64, String, Option<String>, String) {
    for item in prior.iter().rev() {
        if let RolloutItem::Compacted(c) = item {
            let window_number = c.window_number.unwrap_or(1);
            let first = c
                .first_window_id
                .clone()
                .unwrap_or_else(|| "reconcile-first".into());
            let window_id = c
                .window_id
                .clone()
                .unwrap_or_else(|| format!("reconcile-win-{window_number}"));
            return (
                window_number,
                first,
                c.previous_window_id.clone(),
                window_id,
            );
        }
    }
    (1, "reconcile-first".into(), None, "reconcile-win-1".into())
}

async fn latest_durable_marker_message(thread_id: &str, root: Option<&Path>) -> Option<String> {
    let callbacks = lhc_inference_callbacks(false).ok()?;
    let (session, _) = LhcSession::open_with_inference(thread_id, None, root, callbacks).await?;
    let events = session.list_events().await.ok()?;
    let mut best: Option<(i64, String)> = None;
    for ev in events {
        let Some(tp) = ev.text_payload() else {
            continue;
        };
        if CompactMarker::is_durable_writeback_record(&tp.text)
            || tp.text.contains("lhc_compact_marker ")
            || tp.text.starts_with("lhc_compact_marker ")
        {
            let order = ev.event_order();
            if best.as_ref().is_none_or(|(o, _)| order >= *o) {
                // Prefer durable form when we only have a summary note.
                let msg = if CompactMarker::is_durable_writeback_record(&tp.text) {
                    tp.text.clone()
                } else if let Some(point) = compact_point_from_boundary_message(&tp.text) {
                    format!(
                        "lhc_compact_durable {}",
                        serde_json::json!({
                            "viewId": "reconcile",
                            "coveredFrom": 0,
                            "compactPoint": point,
                            "totalTokens": 0,
                            "tailTokens": 0,
                            "firstKeptMessageId": null,
                            "profile": null,
                            "bands": null,
                            "viewMapSeam": crate::compact_bridge::VIEW_MAP_SEAM_ID,
                            "bodyItemCount": 0,
                            "markerKey": format!("codex:{thread_id}:compact_marker:reconcile:{point}"),
                            "derivedContentDigests": [],
                            "derivedHostIds": [],
                            "archiveTip": "reconcile",
                        })
                    )
                } else {
                    continue;
                };
                best = Some((order, msg));
            }
        }
    }
    session.close().await;
    best.map(|(_, m)| m)
}

#[cfg(test)]
#[path = "rollout_reconcile_tests.rs"]
mod tests;
