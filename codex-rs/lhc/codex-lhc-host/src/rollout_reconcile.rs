//! Startup rollout reconciliation (slice E).
//!
//! LHC's SQLite is the source of truth; the rollout is a regenerable projection.
//! At session open, classify the file against the thread and regenerate via
//! materialize + atomic swap when MISSING / CORRUPT / STALE.
//!
//! Fail-open: if the LHC thread is unavailable, leave the file alone (native
//! behavior). Loud `info!` logs name which state triggered a rewrite.

use std::path::Path;

use codex_protocol::ThreadId;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use tracing::info;
use tracing::warn;

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
    let callbacks = lhc_inference_callbacks(false).ok()?;
    let (session, _) =
        LhcSession::open_with_inference(thread_id, None, Some(root_buf.as_path()), callbacks)
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

/// Classify + regenerate when needed. Fail-open if the thread is unavailable.
///
/// Loud `info!` names the trigger state on every rewrite.
pub async fn reconcile_rollout_at_path(
    path: &Path,
    thread_id: &str,
    root: Option<&Path>,
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
        return ReconcileOutcome::Unchanged { reason: "ok" };
    };

    match regenerate_rollout_from_thread(path, thread_id, root, trigger).await {
        Ok(items) => {
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
) -> Result<usize, String> {
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
            // Minimal durable-shaped note so resume can reseed; compact_point 0
            // when no marker is in the archive yet.
            format!(
                "lhc_compact_durable {}",
                serde_json::json!({
                    "viewId": "reconcile",
                    "coveredFrom": 0,
                    "compactPoint": 0,
                    "totalTokens": 0,
                    "tailTokens": 0,
                    "firstKeptMessageId": null,
                    "profile": null,
                    "bands": null,
                    "viewMapSeam": crate::compact_bridge::VIEW_MAP_SEAM_ID,
                    "bodyItemCount": 0,
                    "markerKey": format!("codex:{thread_id}:compact_marker:reconcile"),
                    "derivedContentDigests": [],
                    "derivedHostIds": [],
                    "archiveTip": "reconcile",
                })
            )
        });

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create rollout parent {}: {e}", parent.display()))?;
    }

    let result = materialize_rollout(&MaterializeInput {
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
        live_identity: None,
    });

    for note in &result.gap_notes {
        warn!(%note, ?trigger, "LHC reconcile materialize gap_note");
    }

    atomic_rewrite_rollout(path, &result.items)
        .map_err(|e| format!("atomic rewrite {}: {e}", path.display()))?;

    Ok(result.items.len())
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
