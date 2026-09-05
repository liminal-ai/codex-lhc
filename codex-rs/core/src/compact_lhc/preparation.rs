//! Capture readiness, PreTurn/manual preparation, and production callbacks.
//! Producing an SDK view does not make it the authoritative host generation.

use super::*;

/// LIM-134: how long a **required** PreTurn / Standalone strict compact waits
/// for the asynchronous capture open before failing visibly.
///
/// Ordinary thread startup stays off the critical path; this bound is only
/// consumed once strict compact is known to be required. Expiry is a hard
/// visible compact failure — never `Ok(None)`, never native fallback.
pub(crate) const STRICT_COMPACT_READINESS_BOUND: Duration = Duration::from_secs(60);

/// Required strict compact waits for the capture slot to settle (LIM-134).
///
/// PreTurn / Standalone only: MidTurn keeps its distinct settled in-flight
/// policy and never reaches this wait. Ready continues through the existing
/// strict arm; `Failed`, `Stopped`, and bound expiry are hard visible compact
/// failures that preserve the prior body and issue zero provider requests;
/// turn cancellation aborts the turn without native fallback.
async fn await_capture_ready_for_required_compact(
    slot: &LhcCaptureSlot,
    cancellation_token: &CancellationToken,
) -> Result<codex_lhc_host::CaptureHandle, LhcCompactAttempt> {
    match slot.state() {
        CaptureState::Ready(handle) => return Ok(handle),
        CaptureState::Failed(reason) => {
            return Err(failed_attempt(format!("LHC capture open failed: {reason}")));
        }
        CaptureState::Stopped => {
            return Err(failed_attempt(
                "LHC capture has shut down; compact cannot run",
            ));
        }
        CaptureState::Opening => {}
    }
    let bound = STRICT_COMPACT_READINESS_BOUND;
    info!(target: "codex_core::compact_lhc",
        bound_ms = bound.as_millis() as u64,
        "LHC capture is still opening; required strict compact waits for readiness"
    );
    let settled = tokio::select! {
        biased;
        () = cancellation_token.cancelled() => {
            return Err(cancelled_attempt(
                "turn cancelled while waiting for the LHC capture open",
            ));
        }
        settled = slot.await_settled(bound) => settled,
    };
    match settled {
        Some(CaptureState::Ready(handle)) => Ok(handle),
        Some(CaptureState::Failed(reason)) => {
            Err(failed_attempt(format!("LHC capture open failed: {reason}")))
        }
        Some(CaptureState::Stopped) => Err(failed_attempt(
            "LHC capture has shut down; compact cannot run",
        )),
        Some(CaptureState::Opening) | None => Err(failed_attempt(format!(
            "LHC capture did not open within {}s; compact cannot run",
            bound.as_secs_f64()
        ))),
    }
}

pub(super) fn configured_band_percentages(turn_context: &TurnContext) -> LhcBandPercentages {
    let percentages = turn_context.config.lhc_compact.percentages;
    LhcBandPercentages {
        full: percentages.full,
        smooth: percentages.smooth,
        detailed: percentages.detailed,
        brief: percentages.brief,
    }
}

/// N3: the arm, bound to the **turn's own** cancellation token.
///
/// When the user aborts a turn, produce must stop — no partial install, no
/// marker. Cancellation is cancellation, not permission to compact natively.
/// Background capture-session derivation may continue (session work); the
/// abandoned compact worker is cancelled via the shared flag.
pub(crate) async fn try_run_lhc_compact_arm_with_callbacks_and_cancel(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    initial_context_injection: InitialContextInjection,
    manual: bool,
    callbacks: InferenceCallbacks,
    cancellation_token: &CancellationToken,
) -> CodexResult<LhcCompactAttempt> {
    if cancellation_token.is_cancelled() {
        return Ok(cancelled_attempt(
            "turn cancelled before LHC compact started",
        ));
    }
    if !sess.enabled(Feature::LhcCapture) {
        return Ok(failed_attempt("Feature::LhcCapture off"));
    }

    let Some(slot) = sess.services.thread_extension_data.get::<LhcCaptureSlot>() else {
        return Ok(failed_attempt("no LhcCaptureSlot (capture not opened)"));
    };
    // LIM-134: strict compact is already known to be required here, so a slot
    // that is still Opening is waited for (bounded, cancellation-aware) rather
    // than declared not-ready.
    let handle = match await_capture_ready_for_required_compact(&slot, cancellation_token).await {
        Ok(handle) => handle,
        Err(attempt) => return Ok(attempt),
    };

    // Degraded capture is not a hard stop: flush what we can, then rely on
    // archive-coverage validation + host import to retain current content.
    // Flush is bounded — a busy/wedged capture worker must not stall compact
    // before the produce timeout starts.
    if handle.is_degraded() {
        warn!(target: "codex_core::compact_lhc",
            manual,
            "LHC capture is degraded; compact continues (flush + import + coverage check)"
        );
    }
    if !handle.flush_within(COMPACT_FLUSH_BOUND).await {
        warn!(target: "codex_core::compact_lhc",
            manual,
            timeout_ms = COMPACT_FLUSH_BOUND.as_millis() as u64,
            "LHC capture flush did not complete in time; compact continues (import + coverage)"
        );
    }

    // Derivation readiness affects quality only. Do not wait for
    // drain_settled — pending/running/terminal-failed work uses the fallback
    // ladder (less-derived bands, full-fidelity residue).

    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);
    let host_items = sess
        .clone_history()
        .await
        .raw_items()
        .cloned()
        .collect::<Vec<_>>();

    let cancel = Arc::new(AtomicBool::new(false));

    // I2: re-seed slot from durable CompactedItem record (resume / crash window).
    reseed_slot_from_durable_session(sess.as_ref(), &slot).await;

    // Diagnostic like-for-like baseline = current model-context size.
    // Prefer the rollout file's dual-format extract (what resume would rebuild).
    // A larger compact body is reported but never blocks compaction; imperfect
    // reduction is not catastrophic and later compacts can improve it.
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
                warn!(target: "codex_core::compact_lhc", %err, path = %path.display(), "rollout parse for reduction baseline failed");
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
        configured_band_percentages(turn_context),
        cancellation_token,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            if matches!(&err, CompactWorkerError::Cancelled(_)) {
                return Ok(cancelled_attempt(err.to_string()));
            }
            if matches!(&err, CompactWorkerError::TimedOut(_)) {
                // R14 (CX-S1): the produce worker outran its bound. Warn and
                // continue on the current usable body; the next eligible seam
                // retries. The ordinary path has no MidTurn result, so this is
                // a non-stranding outcome that lets the turn complete.
                warn!(target: "codex_core::compact_lhc",
                    %err,
                    manual,
                    "LHC compact worker timed out; turn continues on its current body (retry at next seam)"
                );
                return Ok(LhcCompactAttempt::ContinuedWithoutCompact {
                    reason: err.to_string(),
                });
            }
            warn!(target: "codex_core::compact_lhc", %err, manual, "LHC compact hard failure; preserving history");
            return Ok(failed_attempt(err.to_string()));
        }
    };

    let produce_body = produced.body.clone();
    if produce_body.is_empty() {
        return Ok(failed_attempt("LHC compact produced empty body"));
    }

    let body_token_estimate = estimate_response_items_tokens(&produce_body);
    if body_token_estimate > baseline_tokens {
        warn!(target: "codex_core::compact_lhc",
            body_tokens = body_token_estimate,
            rollout_model_context_tokens = baseline_tokens,
            items_body = produce_body.len(),
            native_append_polluted,
            manual,
            "LHC compact did not reduce this estimate; proceeding with the usable compact"
        );
    }

    let (_initial_context, world_state_baseline) =
        build_compaction_initial_context(sess.as_ref(), &initial_context_injection).await;

    info!(target: "codex_core::compact_lhc",
        body_tokens = body_token_estimate,
        auto_compact_limit = ?turn_context
            .config
            .model_auto_compact_token_limit
            .or_else(|| turn_context.model_info().auto_compact_token_limit()),
        provider_window = ?turn_context.model_context_window(),
        "LHC compact produce-body size (diagnostic only; not a terminal gate)"
    );

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
        /*host_validation*/ None,
        cancellation_token,
    ))
    .await
}

/// Hand the LHC capture slot the **production** derivation callbacks that its
/// background scheduler derives with.
///
/// Resolved once per thread (the models-manager lookup is not free) and only
/// through [`select_production_inference_callbacks`] — the same selection the
/// compact arm uses, so background derivation can never be the path that
/// quietly persists deterministic text into the durable record (J1). If the
/// derivation model is unavailable the capture session's `LateBoundCallbacks`
/// stay unseeded (waiting, not terminal Err). Compact does not wait on that
/// work and does not seed inert Err callbacks into the capture session.
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
            debug!(target: "codex_core::compact_lhc",
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
pub(super) async fn select_production_inference_callbacks(
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
