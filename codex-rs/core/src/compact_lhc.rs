//! LHC compact arm — real `lhc.compact` body + write-back (Chunk 2b redo).
//!
//! Ladder: above `Feature::TokenBudget` (manual + auto). Fail-open when the
//! archive is missing, degraded, or compact fails.
//!
//! Marker is committed only after durable write-back. Body is never re-ingested.
//!
//! # Slice C — rollout rewrite
//!
//! On install the arm materializes a full rollout projection and atomically
//! rewrites the session file (temp → fsync → rename → reopen). There is no
//! append of a `Compacted` record: the boundary lives inside the rewritten
//! sequence. In-memory history is bands (`replacement_history`) + native tail
//! — equal to what resume rebuilds from the rewritten file.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_features::Feature;
use codex_lhc_host::CompactBoundaryMeta;
use codex_lhc_host::CompactMarker;
use codex_lhc_host::DerivedProvenance;
use codex_lhc_host::InferenceCallbacks;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::LhcCompactResult;
use codex_lhc_host::MaterializeInput;
use codex_lhc_host::atomic_rewrite_rollout;
use codex_lhc_host::commit_compact_marker;
use codex_lhc_host::content_identity_digest;
use codex_lhc_host::estimate_response_items_tokens;
use codex_lhc_host::history_from_materialized_items;
use codex_lhc_host::item_stable_id;
use codex_lhc_host::materialize_rollout;
use codex_lhc_host::model_context_token_estimate_from_rollout_items;
use codex_lhc_host::parse_rollout_items;
use codex_lhc_host::produce_lhc_compact_with_provenance;
use codex_lhc_host::read_materialize_surfaces;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::compact::InitialContextInjection;
use crate::compact::build_compaction_initial_context;
use crate::session::context_window::context_window_token_status;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

const COMPACT_THREAD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(crate) enum LhcCompactAttempt {
    Installed {
        /// Served body installed on the host (law-1 reference).
        #[allow(dead_code)]
        body: Vec<ResponseItem>,
        /// Receipt-backed marker (committed after write-back).
        #[allow(dead_code)]
        marker: CompactMarker,
    },
    Unavailable {
        reason: String,
    },
}

// LHC-HOOK: LHC compact arm entry (manual + auto ladders).

/// Slice E startup reconciliation: if the rollout is MISSING / CORRUPT / STALE
/// relative to the LHC thread, regenerate it from the thread **before** history
/// is loaded. Fail-open when the thread is unavailable (native behavior).
///
/// Loud `info!` is emitted by the host with the triggering state name.
/// LHC SDK futures are `!Send` — this hops to a dedicated thread so callers on
/// multi-thread runtimes (app-server) stay `Send`.
// LHC-HOOK: startup rollout reconciliation before history load (slice E)
pub async fn reconcile_rollout_before_history_load(
    rollout_path: &std::path::Path,
    thread_id: &str,
) {
    let path = rollout_path.to_path_buf();
    let tid = thread_id.to_string();
    let root = codex_lhc_host::lhc_root();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawn_result = std::thread::Builder::new()
        .name(format!("lhc-reconcile-{tid}"))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = tx.send(Err(format!("runtime: {err}")));
                    return;
                }
            };
            let outcome = rt.block_on(codex_lhc_host::reconcile_rollout_at_path(
                path.as_path(),
                &tid,
                Some(root.as_path()),
            ));
            let _ = tx.send(Ok((outcome, path, tid)));
        });
    if let Err(err) = spawn_result {
        warn!(%err, "LHC startup reconciliation: spawn failed; fail-open");
        return;
    }
    match rx.await {
        Ok(Ok((outcome, path, tid))) => match outcome {
            codex_lhc_host::ReconcileOutcome::Regenerated { trigger, items } => {
                info!(
                    path = %path.display(),
                    thread_id = %tid,
                    ?trigger,
                    items,
                    "LHC startup reconciliation completed before history load"
                );
            }
            codex_lhc_host::ReconcileOutcome::Unchanged { reason } => {
                debug!(
                    path = %path.display(),
                    thread_id = %tid,
                    reason,
                    "LHC startup reconciliation: no rewrite"
                );
            }
        },
        Ok(Err(err)) => {
            warn!(%err, "LHC startup reconciliation thread error; fail-open");
        }
        Err(_) => {
            warn!("LHC startup reconciliation thread dropped; fail-open");
        }
    }
}

//
// J1: production default is real ModelClient inference (pinned model, lowest
// effort). Deterministic callbacks are never the silent default — tests must
// call [`try_run_lhc_compact_arm_with_callbacks`] or install a cfg(test)
// override. If the real client cannot be resolved, fail open to the native
// ladder (Unavailable) — never substitute canned text.
#[tracing::instrument(level = "info", skip_all, fields(manual = manual))]
pub(crate) async fn try_run_lhc_compact_arm(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    initial_context_injection: InitialContextInjection,
    manual: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<LhcCompactAttempt> {
    let callbacks = match select_production_inference_callbacks(sess.as_ref()).await {
        Ok(c) => c,
        Err(reason) => {
            warn!(
                %reason,
                manual,
                "LHC compact inference unavailable; failing open to native arms"
            );
            return Ok(LhcCompactAttempt::Unavailable { reason });
        }
    };
    try_run_lhc_compact_arm_with_callbacks_and_cancel(
        sess,
        turn_context,
        initial_context_injection,
        manual,
        callbacks,
        cancellation_token,
    )
    .await
}

/// Explicit callback injection — used by offline unit tests with deterministic
/// callbacks. Production entry never reaches this with canned text.
///
/// Runs with a token that is never cancelled; tests that exercise abort use
/// [`try_run_lhc_compact_arm_with_callbacks_and_cancel`].
#[cfg(test)]
pub(crate) async fn try_run_lhc_compact_arm_with_callbacks(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    initial_context_injection: InitialContextInjection,
    manual: bool,
    callbacks: InferenceCallbacks,
) -> CodexResult<LhcCompactAttempt> {
    try_run_lhc_compact_arm_with_callbacks_and_cancel(
        sess,
        turn_context,
        initial_context_injection,
        manual,
        callbacks,
        &CancellationToken::new(),
    )
    .await
}

/// N3: the arm, bound to the **turn's own** cancellation token.
///
/// When the user aborts a turn, derivation must stop — not keep billing
/// inference for a turn nobody is waiting on. Before this the arm only ever saw
/// its own private `AtomicBool`, set solely by `COMPACT_THREAD_TIMEOUT`;
/// `CompactTask::run` bound its token as `_cancellation_token` and
/// `run_auto_compact` had none at all. Production was saved from installing a
/// post-abort compact only by the hard `handle.abort()` 100 ms later
/// (`GRACEFULL_INTERRUPTION_TIMEOUT_MS`), and the detached derivation worker
/// survived that and kept spending.
///
/// Cancellation is a fail-open per law 3: no partial install, no marker, native
/// ladder unaffected.
pub(crate) async fn try_run_lhc_compact_arm_with_callbacks_and_cancel(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    initial_context_injection: InitialContextInjection,
    manual: bool,
    callbacks: InferenceCallbacks,
    cancellation_token: &CancellationToken,
) -> CodexResult<LhcCompactAttempt> {
    if cancellation_token.is_cancelled() {
        return Ok(LhcCompactAttempt::Unavailable {
            reason: "turn cancelled before LHC compact started".into(),
        });
    }
    if !sess.enabled(Feature::LhcCapture) {
        return Ok(LhcCompactAttempt::Unavailable {
            reason: "Feature::LhcCapture off".into(),
        });
    }

    let Some(slot) = sess.services.thread_extension_data.get::<LhcCaptureSlot>() else {
        return Ok(LhcCompactAttempt::Unavailable {
            reason: "no LhcCaptureSlot (capture not opened)".into(),
        });
    };
    let Some(handle) = slot.get() else {
        return Ok(LhcCompactAttempt::Unavailable {
            reason: "capture handle not ready".into(),
        });
    };
    if handle.is_degraded() {
        return Ok(LhcCompactAttempt::Unavailable {
            reason: "capture degraded".into(),
        });
    }

    handle.flush().await;

    // Derivation runs in the background as intake commits (LHC's own scheduler,
    // `SdkMode::Background`). All the arm does is wait, bounded, for it to
    // settle — and fail open if it has not. This replaces the compact-time
    // drain loop, which existed only because the SDK was misconfigured to
    // `Manual` and its scheduler was inert; see FORK.md §"The drain correction".
    let settled = tokio::select! {
        biased;
        () = cancellation_token.cancelled() => {
            return Ok(LhcCompactAttempt::Unavailable {
                reason: "turn cancelled while waiting for background derivation".into(),
            });
        }
        ok = handle.drain_settled(SETTLE_WAIT) => ok,
    };
    if !settled {
        warn!(
            manual,
            wait_s = SETTLE_WAIT.as_secs(),
            "LHC background derivation did not settle in time; failing open"
        );
        return Ok(LhcCompactAttempt::Unavailable {
            reason: format!(
                "background derivation not settled within {}s",
                SETTLE_WAIT.as_secs()
            ),
        });
    }

    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);
    let host_items = sess.clone_history().await.raw_items().to_vec();

    let cancel = Arc::new(AtomicBool::new(false));

    // I2: re-seed slot from durable CompactedItem record (resume / crash window).
    reseed_slot_from_durable_session(sess.as_ref(), &slot).await;

    // F-L4: like-for-like baseline = current model-context size.
    // Prefer the rollout file's dual-format extract (what resume would rebuild).
    // When the live host history is substantially larger (rollout lag, or a
    // test that seeded a tiny pre-rewrite file then grew the host), fall back
    // to the host stream so we do not false-positive NoReduction.
    //
    // Slice E Part 1: NoReduction is a pathology tripwire, not a size optimizer.
    // When the rollout is native-append-polluted (more than one Compacted record
    // — appended Compacted after/atop an LHC rewrite), a NORMALIZATION rewrite
    // proceeds regardless of the size comparison.
    let host_est = estimate_response_items_tokens(&host_items);
    let (rollout_est, native_append_polluted) = match sess.current_rollout_path().await {
        Ok(Some(path)) if path.exists() => match parse_rollout_items(&path) {
            Ok(items) => {
                let polluted = codex_lhc_host::is_native_append_polluted(&items);
                (
                    model_context_token_estimate_from_rollout_items(&items),
                    polluted,
                )
            }
            Err(err) => {
                warn!(%err, path = %path.display(), "rollout parse for reduction baseline failed");
                (0, false)
            }
        },
        _ => (0, false),
    };
    let baseline_tokens = if rollout_est > 0 && rollout_est.saturating_mul(2) >= host_est {
        rollout_est
    } else {
        host_est
    };

    // H1/G3: ids + digests from prior write-backs (process-local + durable reseed).
    let session_derived = DerivedProvenance {
        ids: slot.derived_ids(),
        digests: slot.derived_digests(),
    };
    let produced = match produce_lhc_compact_on_thread(
        thread_id.clone(),
        root.clone(),
        host_items,
        /*import_missing*/ true,
        callbacks,
        Arc::clone(&cancel),
        session_derived,
        cancellation_token,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            warn!(%err, manual, "LHC compact unavailable; failing open to native arms");
            return Ok(LhcCompactAttempt::Unavailable { reason: err });
        }
    };

    let produce_body = produced.body.clone();
    if produce_body.is_empty() {
        return Ok(LhcCompactAttempt::Unavailable {
            reason: "LHC compact produced empty body".into(),
        });
    }

    // F-L4: refuse only genuine pathology — materialized body larger than the
    // current rollout model-context (like-for-like). Equal/smaller installs.
    // Exception (slice E): native-append-polluted multi-Compacted files get a
    // NORMALIZATION rewrite even when body > baseline — the size guard is not a
    // size optimizer.
    let body_token_estimate = estimate_response_items_tokens(&produce_body);
    if body_token_estimate > baseline_tokens {
        if native_append_polluted {
            info!(
                body_tokens = body_token_estimate,
                rollout_model_context_tokens = baseline_tokens,
                items_body = produce_body.len(),
                manual,
                "LHC compact NORMALIZATION rewrite: native-append-polluted rollout \
                 (multiple Compacted records); skipping NoReduction size guard"
            );
        } else {
            let reason = format!(
                "NoReduction: body_tokens={body_token_estimate} \
                 rollout_model_context_tokens={baseline_tokens} \
                 items_body={} (materialized body larger than current model-context)",
                produce_body.len()
            );
            warn!(%reason, manual, "LHC compact body grew vs rollout model-context; failing open");
            return Ok(LhcCompactAttempt::Unavailable { reason });
        }
    }

    let (_initial_context, world_state_baseline) =
        build_compaction_initial_context(sess.as_ref(), &initial_context_injection).await;

    // Token bound against the produce body (window check for served view). R8.
    if let Some(reason) = body_exceeds_window(turn_context, &produce_body) {
        warn!(%reason, "LHC compact body over window; fail open");
        return Ok(LhcCompactAttempt::Unavailable { reason });
    }

    let reference_context_item = match &initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage { .. } => {
            Some(turn_context.to_turn_context_item())
        }
    };

    // LHC-HOOK: rollout rewrite at LHC compact install (slice C)
    // Box the rewrite future so the parent arm body stays under rustc's
    // query-depth limit when nested under run_turn.
    Box::pin(install_lhc_compact_rewrite(
        sess,
        turn_context,
        &slot,
        thread_id,
        root,
        produced.marker,
        world_state_baseline,
        reference_context_item,
        manual,
        cancellation_token,
    ))
    .await
}

/// Materialize → atomic rewrite → in-memory bands+tail install.
///
/// Extracted from the arm entry so the main future stays under the rustc
/// query-depth limit (large async bodies nested under `run_turn` overflow it).
#[allow(clippy::too_many_arguments)]
async fn install_lhc_compact_rewrite(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    slot: &LhcCaptureSlot,
    thread_id: String,
    root: Option<PathBuf>,
    mut marker: CompactMarker,
    world_state_baseline: Option<std::sync::Arc<crate::context::world_state::WorldState>>,
    reference_context_item: Option<codex_protocol::protocol::TurnContextItem>,
    manual: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<LhcCompactAttempt> {
    // LHC SDK futures are !Send — hop to a dedicated thread like produce does.
    let surfaces = match read_materialize_surfaces_on_thread(
        thread_id.clone(),
        root.clone(),
        cancellation_token,
    )
    .await
    {
        Ok(s) => s,
        Err(err) => {
            warn!(%err, manual, "LHC materialize surfaces unavailable; failing open");
            return Ok(LhcCompactAttempt::Unavailable {
                reason: format!("materialize surfaces: {err}"),
            });
        }
    };

    let rollout_path = match sess.current_rollout_path().await {
        Ok(p) => p,
        Err(err) => {
            warn!(%err, "current_rollout_path failed; continuing without rewrite");
            None
        }
    };

    let prior_generation = match rollout_path.as_ref() {
        Some(path) if path.exists() => parse_rollout_items(path).unwrap_or_else(|err| {
            warn!(%err, path = %path.display(), "failed to parse prior rollout; carry-forwards empty");
            Vec::new()
        }),
        _ => Vec::new(),
    };

    let session_meta = prior_generation
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(meta.clone()),
            _ => None,
        })
        .unwrap_or_else(|| SessionMetaLine {
            meta: SessionMeta {
                session_id: sess.session_id(),
                id: sess.thread_id,
                ..SessionMeta::default()
            },
            git: None,
        });

    let (window_number, window_ids) = sess.advance_auto_compact_window().await;

    let world_state_value = world_state_baseline
        .as_ref()
        .map(|ws| ws.snapshot().into_value());

    // Provisional boundary message (host ids filled after history extract).
    let provisional_message = marker.to_durable_writeback_record();
    let mut materialize_result = materialize_rollout(&MaterializeInput {
        session_meta,
        thread_view: &surfaces.thread_view,
        messages: &surfaces.messages,
        turns: &surfaces.turns,
        prior_generation: &prior_generation,
        boundary: CompactBoundaryMeta {
            message: provisional_message,
            window_number,
            first_window_id: window_ids.first_window_id.to_string(),
            previous_window_id: window_ids.previous_window_id.map(|id| id.to_string()),
            window_id: window_ids.window_id.to_string(),
        },
        world_state: world_state_value,
        turn_context: reference_context_item.clone(),
        live_identity: None,
    });

    for note in &materialize_result.gap_notes {
        error!(%note, manual, "LHC materialize gap_note");
    }

    // In-memory history = bands (replacement_history) + native tail — same as
    // resume-from-rewritten-file rebuilds.
    let mut install_history = history_from_materialized_items(&materialize_result.items);
    if install_history.is_empty() {
        return Ok(LhcCompactAttempt::Unavailable {
            reason: "materialize produced empty install history (bands+tail)".into(),
        });
    }
    for item in &mut install_history {
        if item_stable_id(item).is_none()
            && let Some(prefix) = item.id_prefix()
        {
            item.set_id(Some(codex_protocol::ResponseItemId::new(prefix)));
        }
    }
    let assigned_ids: Vec<String> = install_history.iter().filter_map(item_stable_id).collect();
    if assigned_ids.is_empty() {
        return Ok(LhcCompactAttempt::Unavailable {
            reason: "derived provenance: no stable ids for install history".into(),
        });
    }
    let digests: Vec<String> = install_history
        .iter()
        .map(content_identity_digest)
        .collect();
    marker.derived_host_ids = assigned_ids.clone();
    marker.derived_content_digests = digests.clone();
    marker.body_item_count = install_history.len();
    // Final durable record (with host ids) must ride the Compacted.message in
    // the rewritten file — patch the provisional boundary before the swap.
    // Also stamp assigned ids into the file's replacement_history + tail so
    // resume rebuilds the same items the live session holds.
    let durable_message = marker.to_durable_writeback_record();
    patch_materialized_history_ids(&mut materialize_result.items, &install_history);
    for item in &mut materialize_result.items {
        if let RolloutItem::Compacted(compacted) = item {
            compacted.message = durable_message.clone();
        }
    }

    // Rewrite the rollout file (replaces append of Compacted). Failure leaves
    // the old file authoritative; session continues; next compact retries.
    // NO append fallback path.
    if let Some(path) = rollout_path.as_ref() {
        if let Err(err) = sess.flush_rollout().await {
            error!(
                %err,
                path = %path.display(),
                "LHC rollout flush before rewrite failed; continuing with rewrite attempt"
            );
        }
        match atomic_rewrite_rollout(path, &materialize_result.items) {
            Ok(()) => {
                // Reopen the append handle onto the new inode.
                if let Some(live_thread) = sess.live_thread()
                    && let Err(err) = live_thread.reopen_rollout_after_rewrite().await
                {
                    error!(
                        %err,
                        path = %path.display(),
                        "LHC recorder reopen after rewrite failed; subsequent \
                         appends may target the orphaned prior generation"
                    );
                }
                info!(
                    path = %path.display(),
                    items = materialize_result.items.len(),
                    "LHC rollout rewrite installed (atomic swap)"
                );
            }
            Err(err) => {
                error!(
                    %err,
                    path = %path.display(),
                    "LHC rollout rewrite failed; old file remains authoritative; \
                     next compact will retry (no append fallback)"
                );
            }
        }
    } else {
        debug!("LHC compact: no live rollout path; skip rewrite (in-memory install only)");
    }

    // In-memory install — must equal history_from_materialized_items (law 1 /
    // resume equivalence). Does NOT append Compacted to the file.
    let expected_body = install_history.clone();
    sess.install_compacted_history_memory(
        install_history,
        reference_context_item,
        world_state_baseline,
        Some(durable_message),
    )
    .await;
    sess.recompute_token_usage(turn_context).await;

    let installed = sess.clone_history().await;
    let installed_items = installed.raw_items();
    if !response_items_structurally_equal(installed_items, &expected_body) {
        return Err(CodexErr::UnsupportedOperation(format!(
            "LHC compact law-1 violation: host history drifted from materialized \
             bands+tail (host={}, body={})",
            installed_items.len(),
            expected_body.len()
        )));
    }

    // Process-local slot (fast path); durable already on CompactedItem in rewritten file.
    if let Err(err) =
        slot.mark_derived_after_writeback(assigned_ids.iter().cloned(), digests.iter().cloned())
    {
        warn!(%err, manual, "LHC compact failed to record derived provenance on slot");
        return Ok(LhcCompactAttempt::Unavailable {
            reason: format!("derived provenance record failed: {err}"),
        });
    }

    // LHC archive: small constant-size note only (I1). Digests stay off the model path.
    if let Err(err) = commit_marker_on_thread(thread_id, root, marker.clone()).await {
        warn!(
            %err,
            manual,
            "LHC compact archive note commit failed after write-back; failing open \
             (durable derived record already on CompactedItem + slot)"
        );
        return Ok(LhcCompactAttempt::Unavailable {
            reason: format!("marker commit failed after write-back: {err}"),
        });
    }

    info!(
        manual,
        items = expected_body.len(),
        covered_from = marker.covered_from,
        compact_point = marker.compact_point,
        total_tokens = marker.total_tokens,
        derived_ids = marker.derived_host_ids.len(),
        runtime_note_chars = marker.to_runtime_note_text().len(),
        "LHC compact arm installed write-back from real CompactReceipt (rewrite path)"
    );

    Ok(LhcCompactAttempt::Installed {
        body: expected_body,
        marker,
    })
}

/// Bound on waiting for LHC's background scheduler to settle before a compact.
///
/// This is a *wait*, not a work loop — the drain is already running and this
/// only asks when it is done. Kept well under `COMPACT_THREAD_TIMEOUT` so the
/// arm fails open to the native ladder rather than being killed by the caller.
const SETTLE_WAIT: Duration = Duration::from_secs(60);

/// Hand the LHC capture slot the **production** derivation callbacks that its
/// background scheduler derives with.
///
/// Resolved once per thread (the models-manager lookup is not free) and only
/// through [`select_production_inference_callbacks`] — the same selection the
/// compact arm uses, so background derivation can never be the path that
/// quietly persists deterministic text into the durable record (J1). If the
/// derivation model is unavailable the capture session's `LateBoundCallbacks`
/// stay unseeded: queued inference work waits rather than failing terminally,
/// and the arm's bounded settle-wait fails open at compact time.
pub(crate) async fn seed_lhc_derivation_callbacks(sess: &Session) {
    if !sess.enabled(Feature::LhcCapture) {
        return;
    }
    let Some(slot) = sess.services.thread_extension_data.get::<LhcCaptureSlot>() else {
        return;
    };
    if slot.has_derivation_callbacks() {
        return;
    }
    match select_production_inference_callbacks(sess).await {
        Ok(callbacks) => slot.set_derivation_callbacks(callbacks),
        Err(reason) => {
            debug!(
                %reason,
                "LHC background derivation callbacks unavailable; capture stays unseeded"
            );
        }
    }
}

/// Production inference selection: real ModelClient bridge only.
///
/// Under `cfg(test)`, an explicit session override may supply callbacks (for
/// CompactTask / auto-ladder tests that cannot pass callbacks through the
/// production entry). The override is never set for real sessions.
async fn select_production_inference_callbacks(
    sess: &Session,
) -> Result<InferenceCallbacks, String> {
    #[cfg(test)]
    {
        if let Ok(guard) = sess.services.lhc_test_inference.lock()
            && let Some(cbs) = guard.as_ref()
        {
            return Ok(cbs.clone());
        }
    }
    crate::lhc_inference_bridge::try_lhc_model_inference_callbacks(sess).await
}

/// I2/L3: re-seed process slot from durable CompactedItem record.
/// Durable is source of truth — always merge even when the cache is warm.
async fn reseed_slot_from_durable_session(sess: &Session, slot: &LhcCaptureSlot) {
    let Some(msg) = sess.last_lhc_durable_derived_message().await else {
        return;
    };
    let Some(marker) = CompactMarker::parse_durable_writeback_record(&msg) else {
        return;
    };
    slot.ensure_derived_from_durable(
        marker.derived_host_ids.iter().cloned(),
        marker.derived_content_digests.iter().cloned(),
    );
}

/// Stamp host-assigned ids from `install_history` (bands + tail) onto the
/// materialized rollout sequence so the rewritten file and live memory match.
fn patch_materialized_history_ids(items: &mut [RolloutItem], install_history: &[ResponseItem]) {
    let band_len = items
        .iter()
        .find_map(|item| match item {
            RolloutItem::Compacted(c) => c.replacement_history.as_ref().map(Vec::len),
            _ => None,
        })
        .unwrap_or(0);
    let band_len = band_len.min(install_history.len());
    let (bands, tail) = install_history.split_at(band_len);

    let mut past_boundary = false;
    let mut tail_idx = 0usize;
    for item in items.iter_mut() {
        match item {
            RolloutItem::Compacted(c) => {
                c.replacement_history = Some(bands.to_vec());
                past_boundary = true;
            }
            RolloutItem::ResponseItem(response_item) if past_boundary => {
                if let Some(src) = tail.get(tail_idx) {
                    *response_item = src.clone();
                    tail_idx += 1;
                }
            }
            _ => {}
        }
    }
}

fn body_exceeds_window(turn_context: &TurnContext, body: &[ResponseItem]) -> Option<String> {
    let window = turn_context.model_context_window().or_else(|| {
        turn_context
            .config
            .model_auto_compact_token_limit
            .or_else(|| turn_context.model_info.auto_compact_token_limit())
    })?;
    // Cheap char/4 estimate (same order as LHC estimate_tokens).
    let chars: usize = body
        .iter()
        .map(|item| match item {
            ResponseItem::Message { content, .. } => content
                .iter()
                .map(|c| match c {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        text.len()
                    }
                    ContentItem::InputImage { image_url, .. } => image_url.len(),
                    ContentItem::InputAudio { audio_url } => audio_url.len(),
                })
                .sum::<usize>(),
            _ => 64,
        })
        .sum();
    let est_tokens = (chars / 4) as i64;
    if est_tokens > window {
        Some(format!(
            "estimated body tokens {est_tokens} exceed context window {window}"
        ))
    } else {
        None
    }
}

/// Read materialize surfaces on a dedicated thread (LHC SDK futures are !Send).
async fn read_materialize_surfaces_on_thread(
    thread_id: String,
    root: Option<PathBuf>,
    turn_cancel: &CancellationToken,
) -> Result<codex_lhc_host::MaterializeSurfaces, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let join = std::thread::Builder::new()
        .name(format!("lhc-materialize-{thread_id}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("runtime: {e}"))?;
                rt.block_on(
                    async move { read_materialize_surfaces(&thread_id, root.as_deref()).await },
                )
            }))
            .unwrap_or_else(|payload| {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic in materialize surfaces thread".into());
                Err(msg)
            });
            let _ = tx.send(result);
        })
        .map_err(|e| format!("spawn materialize surfaces thread: {e}"))?;

    let result = tokio::select! {
        biased;
        () = turn_cancel.cancelled() => {
            // Detach: join will finish; we fail open.
            return Err("turn cancelled while reading materialize surfaces".into());
        }
        r = rx => r.map_err(|_| "materialize surfaces thread dropped".to_string())?,
    };
    // Best-effort join so we don't leak threads on the happy path.
    let _ = join.join();
    result
}

/// Structural equality for law 1: role + each content variant (incl. media URLs).
pub(crate) fn response_items_structurally_equal(a: &[ResponseItem], b: &[ResponseItem]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| match (x, y) {
        (
            ResponseItem::Message {
                role: r1,
                content: c1,
                phase: p1,
                ..
            },
            ResponseItem::Message {
                role: r2,
                content: c2,
                phase: p2,
                ..
            },
        ) => r1 == r2 && p1 == p2 && content_items_equal(c1, c2),
        (x, y) => format!("{x:?}") == format!("{y:?}"),
    })
}

fn content_items_equal(a: &[ContentItem], b: &[ContentItem]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| match (x, y) {
        (ContentItem::InputText { text: t1 }, ContentItem::InputText { text: t2 }) => t1 == t2,
        (ContentItem::OutputText { text: t1 }, ContentItem::OutputText { text: t2 }) => t1 == t2,
        (
            ContentItem::InputImage {
                image_url: u1,
                detail: d1,
            },
            ContentItem::InputImage {
                image_url: u2,
                detail: d2,
            },
        ) => u1 == u2 && d1 == d2,
        (ContentItem::InputAudio { audio_url: u1 }, ContentItem::InputAudio { audio_url: u2 }) => {
            u1 == u2
        }
        _ => false,
    })
}

#[allow(clippy::too_many_arguments)]
async fn produce_lhc_compact_on_thread(
    thread_id: String,
    root: Option<PathBuf>,
    host_items: Vec<ResponseItem>,
    import_missing: bool,
    callbacks: codex_lhc_host::InferenceCallbacks,
    cancel: Arc<AtomicBool>,
    session_derived: DerivedProvenance,
    turn_cancel: &CancellationToken,
) -> Result<LhcCompactResult, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cancel_thread = Arc::clone(&cancel);
    let join = std::thread::Builder::new()
        .name(format!("lhc-compact-{thread_id}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("runtime: {e}"))?;
                rt.block_on(async move {
                    produce_lhc_compact_with_provenance(
                        &thread_id,
                        root.as_deref(),
                        &host_items,
                        import_missing,
                        callbacks,
                        Some(cancel_thread),
                        &session_derived,
                    )
                    .await
                    .map_err(|e| e.to_string())
                })
            }));
            let out = match result {
                Ok(inner) => inner,
                Err(payload) => {
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "lhc-compact thread panicked".into()
                    };
                    Err(msg)
                }
            };
            let _ = tx.send(out);
        })
        .map_err(|e| format!("spawn lhc-compact thread: {e}"))?;

    let thread_timeout = COMPACT_THREAD_TIMEOUT;
    // N3: the turn's own cancellation races the worker and the timeout. The
    // detached worker checks `cancel` between its compact steps (event import,
    // compact, context fetch, mapping), so setting it here stops the abandoned
    // attempt at the next step boundary instead of letting it run to the end.
    let raced = tokio::select! {
        biased;
        () = turn_cancel.cancelled() => {
            cancel.store(true, Ordering::SeqCst);
            drop(join);
            warn!("LHC compact cancelled by turn abort; stopping derivation and failing open");
            return Err("lhc-compact cancelled by turn abort".into());
        }
        r = tokio::time::timeout(thread_timeout, rx) => r,
    };
    match raced {
        Ok(Ok(r)) => {
            // Success path: surface panics without hanging the turn forever.
            // Bound the join so a stuck thread cannot pin the worker.
            match tokio::time::timeout(Duration::from_secs(5), async {
                tokio::task::spawn_blocking(move || join.join())
                    .await
                    .map_err(|e| format!("join spawn_blocking failed: {e}"))
            })
            .await
            {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(_))) => {
                    if r.is_ok() {
                        return Err("lhc-compact thread panicked after success".into());
                    }
                }
                Ok(Err(e)) => warn!(%e, "join spawn_blocking failed"),
                Err(_) => {
                    // Detach: drop JoinHandle (detaches thread) rather than block.
                    warn!("lhc-compact join timed out after success path; detaching thread");
                }
            }
            r
        }
        Ok(Err(_)) => {
            cancel.store(true, Ordering::SeqCst);
            // Detach rather than join — timeout must bound the caller (F4).
            drop(join);
            Err("lhc-compact thread dropped".into())
        }
        Err(_) => {
            cancel.store(true, Ordering::SeqCst);
            // F4: do not await join on the timeout path — leak/detach the thread
            // so the turn can fail-open. A leaked thread beats a hung session.
            drop(join);
            warn!(
                timeout_ms = thread_timeout.as_millis() as u64,
                "lhc-compact timed out; detaching worker thread and failing open"
            );
            Err(format!(
                "lhc-compact timed out after {}ms",
                thread_timeout.as_millis()
            ))
        }
    }
}

async fn commit_marker_on_thread(
    thread_id: String,
    root: Option<PathBuf>,
    marker: CompactMarker,
) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("lhc-marker-{thread_id}"))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("runtime: {e}")));
                    return;
                }
            };
            let r = rt.block_on(commit_compact_marker(&thread_id, root.as_deref(), &marker));
            let _ = tx.send(r);
        })
        .map_err(|e| format!("spawn marker thread: {e}"))?;
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("marker thread dropped".into()),
        Err(_) => Err("marker commit timed out".into()),
    }
}

/// Used by law-2 next-turn test.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn token_limit_reached(sess: &Session, turn_context: &TurnContext) -> bool {
    context_window_token_status(sess, turn_context)
        .await
        .token_limit_reached
}

#[cfg(test)]
#[path = "compact_lhc_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "compact_lhc_slice_d_tests.rs"]
mod slice_d_tests;
