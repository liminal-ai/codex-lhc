//! Host installation coordination: materialize/validate, durable swap, then memory.
//!
//! The prior rollout remains authoritative until the new generation is proven
//! active. After that point, finish the matching host install or deny sampling;
//! a reopen error cannot roll back a proven durable compact. Marker publication
//! follows installation and keeps its existing bounded retry policy.

use super::*;

/// Materialize → atomic rewrite → in-memory bands+tail install.
///
/// Extracted from the arm entry so the main future stays under the rustc
/// query-depth limit (large async bodies nested under `run_turn` overflow it).
#[allow(clippy::too_many_arguments)]
pub(super) async fn install_lhc_compact_rewrite(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    slot: &LhcCaptureSlot,
    thread_id: String,
    root: Option<PathBuf>,
    mut marker: CompactMarker,
    world_state_baseline: Option<std::sync::Arc<crate::context::world_state::WorldState>>,
    reference_context_item: Option<codex_protocol::protocol::TurnContextItem>,
    manual: bool,
    host_validation: Option<codex_lhc_host::BodyValidationSpec>,
    cancellation_token: &CancellationToken,
) -> CodexResult<LhcCompactAttempt> {
    // LHC SDK futures are !Send — hop to a dedicated thread like produce does.
    if cancellation_token.is_cancelled() {
        return Ok(cancelled_attempt(
            "turn cancelled before LHC compact install",
        ));
    }

    let surfaces = match read_materialize_surfaces_on_thread(
        thread_id.clone(),
        root.clone(),
        cancellation_token,
    )
    .await
    {
        Ok(s) => s,
        Err(err) => {
            if matches!(&err, CompactWorkerError::Cancelled(_)) {
                return Ok(cancelled_attempt(err.to_string()));
            }
            warn!(target: "codex_core::compact_lhc", %err, manual, "LHC materialize surfaces unavailable; hard stop");
            return Ok(failed_attempt(format!("materialize surfaces: {err}")));
        }
    };

    // R18 (CX-S2): a rollout path lookup failure degrades to an in-memory-only
    // install instead of failing the compact. Ok(None) is the intentional
    // ephemeral / non-persistent session contract and takes the same path. The
    // durable source for next-open recovery is the installed LHC thread view
    // plus the captured canonical tail; reconciliation rewrites the file when a
    // path is available again.
    let rollout_path = match sess.current_rollout_path().await {
        Ok(p) => p,
        Err(err) => {
            warn!(target: "codex_core::compact_lhc",
                %err,
                "current_rollout_path failed; installing in memory only (LHC thread view stays the durable source)"
            );
            None
        }
    };

    let prior_generation = match rollout_path.as_ref() {
        Some(path) if path.exists() => parse_rollout_items(path).unwrap_or_else(|err| {
            warn!(target: "codex_core::compact_lhc", %err, path = %path.display(), "failed to parse prior rollout; carry-forwards empty");
            Vec::new()
        }),
        _ => Vec::new(),
    };

    // M2: eligible non-inherited paginated realtime rows must survive this
    // rewrite; they need the ordinal-bearing authority read, not the
    // skip-tolerant item-only one. An unprovable set is a refusal: keep the
    // prior body rather than installing a replacement that silently dropped
    // rows the reader could not prove.
    let prior_realtime_items = match rollout_path.as_ref() {
        Some(path) if path.exists() => match codex_lhc_host::parse_prior_realtime_items(path) {
            Ok(items) => items,
            Err(err) => {
                error!(target: "codex_core::compact_lhc",
                    %err,
                    path = %path.display(),
                    manual,
                    "LHC compact refusing to install: prior rollout realtime rows cannot be proven"
                );
                return Ok(kept_prior_body_attempt(format!(
                    "prior rollout realtime rows unreadable; prior body kept: {err}"
                )));
            }
        },
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

    // Plan window advance; commit only after successful replacement so failed
    // construction/rewrite leaves IDs, number, prefill, and one-shot flags alone.
    let (window_number, window_ids) = sess.plan_auto_compact_window_advance().await;

    let world_state_value = world_state_baseline
        .as_ref()
        .map(|ws| serde_json::Value::Object(ws.snapshot().into_object()));

    // Provisional boundary message (host ids filled after history extract).
    let provisional_message = marker.to_durable_writeback_record();
    let mut materialize_result = materialize_rollout(&MaterializeInput {
        session_meta,
        thread_view: &surfaces.thread_view,
        messages: &surfaces.messages,
        turns: &surfaces.turns,
        prior_generation: &prior_generation,
        prior_realtime_items: &prior_realtime_items,
        boundary: CompactBoundaryMeta {
            message: provisional_message,
            window_number,
            first_window_id: window_ids.first_window_id.to_string(),
            previous_window_id: window_ids.previous_window_id.map(|id| id.to_string()),
            window_id: window_ids.window_id.to_string(),
        },
        world_state: world_state_value,
        turn_context: reference_context_item.clone(),
        // Live identity from the same label sources capture uses
        // (config.model / config.model_provider_id), so same-identity replay
        // actually re-emits encrypted reasoning (R2 host gate).
        live_identity: Some(codex_lhc_host::ModelIdentity::new(
            turn_context.config.model_provider_id.clone(),
            turn_context
                .config
                .model
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            codex_lhc_host::ModelIdentity::RESPONSES_API,
        )),
    });

    for note in &materialize_result.gap_notes {
        error!(target: "codex_core::compact_lhc", %note, manual, "LHC materialize gap_note");
    }

    // M1: a captured item the rebuilt sequence cannot represent exactly is a
    // visible refusal, not a degradation ladder input. Refuse before anything
    // is installed: the session keeps the body it already holds (same
    // disposition as R19) rather than serving a silently altered item.
    if !materialize_result.refusals.is_empty() {
        for refusal in &materialize_result.refusals {
            error!(target: "codex_core::compact_lhc",
                %refusal,
                manual,
                "LHC compact refusing to install: materialization cannot represent a \
                 captured item exactly"
            );
        }
        return Ok(kept_prior_body_attempt(format!(
            "materialization cannot represent captured items exactly; prior body kept: {}",
            materialize_result.refusals.join("; ")
        )));
    }

    // In-memory history = bands (replacement_history) + native tail — same as
    // resume-from-rewritten-file rebuilds.
    let mut install_history = history_from_materialized_items(&materialize_result.items);
    if install_history.is_empty() {
        // R19 (CX-S3): an empty materialization is not a reason to strand. The
        // session keeps the body it already holds, the turn continues, and
        // compact retries at the next seam.
        warn!(target: "codex_core::compact_lhc",
            manual,
            materialized_items = materialize_result.items.len(),
            messages = surfaces.messages.len(),
            turns = surfaces.turns.len(),
            "LHC compact materialized an empty install history (bands+tail); \
             keeping the prior body and continuing without compact"
        );
        return Ok(kept_prior_body_attempt(
            "materialize produced empty install history (bands+tail); prior body kept",
        ));
    }
    // LIM-69: materialize reconstructs CustomToolCall(Output) from portable
    // LHC messages and drops status / ContentItems / name. Graft the exact
    // live pair (id + host metadata stripped) so validation, rewrite, and
    // in-memory install share the same provider-stable bytes.
    //
    // R8 (CX-S3): a pair that cannot be proven keeps the LHC-reconstructed
    // call/output instead of blocking — same call_id, same correlation, only
    // provider-specific fields (status, namespace) missing. Degraded body,
    // valid request, loud warning.
    if let Some(spec) = host_validation.as_ref()
        && !spec.protected_tool_call_ids.is_empty()
    {
        let live_items: Vec<ResponseItem> =
            sess.clone_history().await.raw_items().cloned().collect();
        let graft = codex_lhc_host::graft_live_protected_pairs(
            &mut install_history,
            &live_items,
            &spec.protected_tool_call_ids,
        );
        if graft.is_fully_grafted() {
            info!(target: "codex_core::compact_lhc",
                attempt_id = %spec.attempt_id,
                grafted = graft.grafted.len(),
                "LHC MidTurn grafted live protected pairs into materialized body"
            );
        } else {
            warn!(target: "codex_core::compact_lhc",
                attempt_id = %spec.attempt_id,
                grafted = graft.grafted.len(),
                degraded = graft.degraded.len(),
                detail = %graft.degraded_summary(),
                "LHC MidTurn protected-pair graft could not prove every pair; continuing \
                 with the LHC-reconstructed pair (same call_id and correlation, \
                 provider-specific fields may be absent)"
            );
        }
    }

    // Size is diagnostic only. Structural host-validation still runs below.
    let install_tokens = estimate_response_items_tokens(&install_history);
    info!(target: "codex_core::compact_lhc",
        body_tokens = install_tokens,
        auto_compact_limit = ?turn_context
            .config
            .model_auto_compact_token_limit
            .or_else(|| turn_context.model_info().auto_compact_token_limit()),
        provider_window = ?turn_context.model_context_window(),
        manual,
        "LHC compact install-history size (diagnostic only; not a terminal gate)"
    );

    // LIM-67 host full-body validation (protected escalation only).
    // `install_history` is the exact item sequence the next provider request
    // serves (identical to what the rewrite and in-memory install use).
    //
    // R10 (CX-S3): validation detects, it does not veto. A body that fails is
    // degraded to the best version still legal to send — unpaired/orphan items
    // dropped, oversized content truncated, missing encrypted reasoning
    // omitted — and the same drops are applied to the materialized rollout
    // items so the rewritten file rebuilds exactly the installed body (law 1).
    // The provider is the final authority on what it accepts; a rejected
    // request is recoverable, a stranded session is not.
    //
    // R11 (CX-S3): the durable acknowledgment is a receipt. It records that
    // this attempt's view is the one being served — including *how* it
    // degraded — and a write failure never decides whether compact proceeds.
    if let Some(spec) = host_validation.as_ref() {
        #[cfg(any(test, feature = "test-util"))]
        let validation = if slot.mid_turn_test_force_body_validation_fail() {
            Err("test-injected host body validation failure".to_string())
        } else {
            codex_lhc_host::validate_next_request_body(&install_history, spec)
        };
        #[cfg(not(any(test, feature = "test-util")))]
        let validation = codex_lhc_host::validate_next_request_body(&install_history, spec);
        let ack_reason = match validation {
            Ok(report) => {
                info!(target: "codex_core::compact_lhc",
                    attempt_id = %spec.attempt_id,
                    body_items = report.body_item_count,
                    body_tokens = report.body_token_estimate,
                    threshold = ?report.safe_runway_threshold_tokens,
                    protected_pairs = report.protected_pair_count,
                    reasoning_preserved = report.reasoning_preserved_count,
                    "LHC MidTurn host full-body validation ok; proceeding to rewrite"
                );
                None
            }
            Err(reason) => {
                let degraded =
                    codex_lhc_host::degrade_body_to_best_available(&install_history, spec);
                let detail = degraded.summary();
                warn!(target: "codex_core::compact_lhc",
                    attempt_id = %spec.attempt_id,
                    %reason,
                    body_items_before = install_history.len(),
                    body_items_after = degraded.body.len(),
                    dropped_items = degraded.dropped_count(),
                    degradations = degraded.degradations.len(),
                    %detail,
                    "LHC MidTurn host full-body validation failed; degrading to the best \
                     available body and continuing (never stranding; the provider is the \
                     final authority on the request)"
                );
                if degraded.dropped_count() > 0 {
                    drop_materialized_items(&mut materialize_result.items, &degraded.kept);
                }
                install_history = degraded.body;
                if install_history.is_empty() {
                    // Nothing survived the ladder: same disposition as R19 —
                    // keep the body the session already holds.
                    warn!(target: "codex_core::compact_lhc",
                        attempt_id = %spec.attempt_id,
                        %detail,
                        "LHC MidTurn degrade ladder emptied the body; keeping the prior body \
                         and continuing without compact"
                    );
                    return Ok(kept_prior_body_attempt(format!(
                        "degraded body empty after validation failure ({reason}); prior body kept"
                    )));
                }
                Some(format!("proceeded degraded: {reason} | {detail}"))
            }
        };
        // `ok` records what is true after the ladder: this attempt's view is
        // the body being served, degradations and all. Recording `failed`
        // would gate rollout regeneration for a session that did compact —
        // exactly the bookkeeping-as-authority pattern R10/R11 remove.
        #[cfg(any(test, feature = "test-util"))]
        let ack_write = if slot.mid_turn_test_force_validation_ack_write_fail() {
            Err("validation ack write failed (test injection)".to_string())
        } else {
            record_host_validation_on_thread(
                thread_id.clone(),
                root.clone(),
                spec.attempt_id.clone(),
                /*ok*/ true,
                ack_reason,
            )
            .await
        };
        #[cfg(not(any(test, feature = "test-util")))]
        let ack_write = record_host_validation_on_thread(
            thread_id.clone(),
            root.clone(),
            spec.attempt_id.clone(),
            /*ok*/ true,
            ack_reason,
        )
        .await;
        if let Err(err) = ack_write {
            warn!(target: "codex_core::compact_lhc",
                %err,
                attempt_id = %spec.attempt_id,
                "LHC MidTurn host validation ack write failed; the body is installed anyway \
                 (receipts observe, never govern)"
            );
        }
    }

    for item in &mut install_history {
        if item_stable_id(item).is_none()
            && let Some(prefix) = item.id_prefix()
        {
            item.set_id(Some(codex_protocol::ResponseItemId::new(prefix)));
        }
    }
    let assigned_ids: Vec<String> = install_history.iter().filter_map(item_stable_id).collect();
    let digests: Vec<String> = install_history
        .iter()
        .map(content_identity_digest)
        .collect();
    if assigned_ids.is_empty() {
        // R9 (CX-S3): stable ids are the preferred identity, not the only one.
        // Content digests are computed unconditionally for every item, so
        // resume equivalence and coverage accounting survive on digests alone.
        warn!(target: "codex_core::compact_lhc",
            manual,
            body_items = install_history.len(),
            digests = digests.len(),
            "LHC compact derived provenance has no assignable stable ids; \
             falling through to content-digest identity"
        );
    }
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
    // the old file authoritative and does not commit the planned window.
    // NO append fallback / native Compacted. When no path is available the
    // install is in-memory only (R18) and reconciliation rewrites at next open.
    if let Some(path) = rollout_path.as_ref() {
        if let Err(err) = sess.flush_rollout().await {
            error!(target: "codex_core::compact_lhc",
                %err,
                path = %path.display(),
                "LHC rollout flush before rewrite failed; continuing with rewrite attempt"
            );
        }
        // Exact prior-generation identity (turn parts, Story 5 M2 residual):
        // the bytes of the authoritative active file right now, after the
        // final flush and before the swap. `.prev` is this inode renamed, so
        // only a byte-exact match later proves a file is the prior
        // generation. If it cannot be captured, nothing is swapped: the old
        // file stays authoritative and the seam retries later.
        let prior_bytes = if path.exists() {
            match std::fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    error!(target: "codex_core::compact_lhc",
                        %err,
                        path = %path.display(),
                        "LHC rollout unreadable before rewrite; not swapping; \
                         preserving in-memory history (no native compact)"
                    );
                    return Ok(failed_attempt(format!(
                        "prior rollout unreadable before rewrite: {err}"
                    )));
                }
            }
        } else {
            None
        };
        // Interrupted-swap reconciliation: an error out of the swap does not
        // say which generation is active. Classify the actual on-disk state
        // under this arm's one-writer authority against the exact identities
        // of both generations (prior bytes; the new items' wire content) and
        // establish exactly one authoritative generation before deciding: old
        // still active → retry later; old moved and the proven new generation
        // at tmp → finish the swap (proven durable); new already active
        // (post-rename fsync or hook error) → the compact stands and the host
        // must complete its matching in-memory / window install, never roll
        // it back; anything unproven → deny sampling with the exact state.
        // The generation identity this attempt writes is generated here and
        // retained across the attempt; the proof compares against it exactly
        // and never reads it back from disk.
        let generation_id = codex_lhc_host::new_rollout_generation_id();
        let reconciled_after_error = match atomic_rewrite_rollout_as_generation(
            path,
            &materialize_result.items,
            &generation_id,
        ) {
            Ok(()) => None,
            Err(err) => {
                let generations = codex_lhc_host::SwapGenerations {
                    prior_bytes: prior_bytes.as_deref(),
                    new_items: &materialize_result.items,
                    new_generation_id: &generation_id,
                };
                match codex_lhc_host::reconcile_interrupted_swap(path, generations) {
                    codex_lhc_host::SwapReconciliation::OldActive => {
                        error!(target: "codex_core::compact_lhc",
                            %err,
                            path = %path.display(),
                            "LHC rollout rewrite failed; old file remains authoritative; \
                             preserving in-memory history (no native compact)"
                        );
                        return Ok(failed_attempt(format!("rollout rewrite failed: {err}")));
                    }
                    codex_lhc_host::SwapReconciliation::RestoredOld { detail } => {
                        error!(target: "codex_core::compact_lhc",
                            %err,
                            %detail,
                            path = %path.display(),
                            "LHC rollout rewrite failed after moving the old generation; \
                             restored it as the authoritative active file; \
                             preserving in-memory history (no native compact)"
                        );
                        return Ok(failed_attempt(format!(
                            "rollout rewrite failed: {err}; {detail}"
                        )));
                    }
                    codex_lhc_host::SwapReconciliation::Unreconciled { detail } => {
                        error!(target: "codex_core::compact_lhc",
                            %err,
                            %detail,
                            path = %path.display(),
                            "LHC rollout rewrite failed and no single authoritative generation \
                             could be established; denying further sampling on this rollout"
                        );
                        return Ok(LhcCompactAttempt::RolloutUnreconciled {
                            reason: format!("rollout rewrite failed: {err}; {detail}"),
                        });
                    }
                    codex_lhc_host::SwapReconciliation::NewActive { finished_here } => {
                        warn!(target: "codex_core::compact_lhc",
                            %err,
                            finished_here,
                            path = %path.display(),
                            "LHC rollout rewrite reported an error but the new generation is \
                             the active file; completing the host install against it"
                        );
                        Some(finished_here)
                    }
                }
            }
        };
        match reconciled_after_error {
            Some(_) | None => {
                // Reopen the append handle onto the new inode (retry once).
                if let Some(live_thread) = sess.live_thread() {
                    let reopen = live_thread.reopen_rollout_after_rewrite().await;
                    let reopen = match reopen {
                        Ok(()) => Ok(()),
                        Err(err) => {
                            warn!(target: "codex_core::compact_lhc",
                                %err,
                                path = %path.display(),
                                "LHC recorder reopen after rewrite failed; retrying once"
                            );
                            live_thread.reopen_rollout_after_rewrite().await
                        }
                    };
                    if let Err(err) = reopen {
                        // R12 (CX-S2): the compacted rollout was written and
                        // fsynced — it stands. Restoring the oversized prior
                        // generation would throw away a completed compact to
                        // protect an append handle. Instead record what the
                        // next open needs to reconcile the appends that will
                        // not land in this file: which rollout is live, how far
                        // the recorder got, and where canonical LHC capture is.
                        error!(target: "codex_core::compact_lhc",
                            %err,
                            path = %path.display(),
                            "LHC recorder reopen after rewrite failed after retry; \
                             compacted rollout stands, later appends may be lost to the orphan inode"
                        );
                        persist_reopen_failure_receipt(
                            path,
                            &thread_id,
                            root.clone(),
                            materialize_result.items.len() as u64,
                            &err.to_string(),
                        )
                        .await;
                    }
                }
                info!(target: "codex_core::compact_lhc",
                    path = %path.display(),
                    items = materialize_result.items.len(),
                    reconciled = reconciled_after_error.is_some(),
                    "LHC rollout rewrite installed (atomic swap)"
                );
            }
        }
    } else {
        debug!(target: "codex_core::compact_lhc", "LHC compact: no live rollout path; skip rewrite (in-memory install only)");
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
    // Commit the planned window only with successful replacement.
    sess.commit_auto_compact_window_advance(window_number, window_ids)
        .await;
    sess.recompute_token_usage(turn_context).await;

    // R13 (CX-S2): law-1 (host history == materialized bands+tail) is checked as
    // an observation, not a gate. The install already happened and the session
    // already holds the new body; a mismatch here is a bug in
    // materialize/install, and aborting the turn after the fact strands a
    // session that just successfully compacted. Log it loudly and continue.
    let installed = sess.clone_history().await;
    let installed_items = installed.raw_items().cloned().collect::<Vec<_>>();
    if !response_items_structurally_equal(&installed_items, &expected_body) {
        error!(target: "codex_core::compact_lhc",
            manual,
            host_items = installed_items.len(),
            body_items = expected_body.len(),
            "LHC compact law-1 mismatch after install: host history drifted from \
             materialized bands+tail; history stays installed (report as a bug)"
        );
    }

    // Validated history is installed. Later marker/provenance bookkeeping
    // failures must not return an outcome that can run another compactor.
    if let Err(err) =
        slot.mark_derived_after_writeback(assigned_ids.iter().cloned(), digests.iter().cloned())
    {
        warn!(target: "codex_core::compact_lhc",
            %err,
            manual,
            "LHC compact: derived provenance slot bookkeeping failed after install; \
             recording degradation (history remains installed)"
        );
    }

    // LHC archive: small constant-size note only (I1). Digests stay off the model path.
    //
    // Write-behind duplication, not the durable record. The durable compact
    // record is the rollout `Compacted` item (fsynced by the rewrite above);
    // next open reseeds derived provenance from it, so a missing archive note
    // never causes re-ingest. The commit still retries briefly — a single
    // archive open under contention is not evidence the archive is wedged —
    // but a wedged archive must not hold the turn.
    if let Err(err) = commit_marker_with_retry(thread_id, root, &marker).await {
        warn!(target: "codex_core::compact_lhc",
            %err,
            manual,
            marker_key = %marker.marker_key,
            "LHC compact archive marker note commit failed after write-back; \
             the compacted body stands and the durable record is in the \
             rollout — next open reseeds provenance from it; only this \
             thread's archive lacks the duplicate note"
        );
    }

    info!(target: "codex_core::compact_lhc",
        manual,
        items = expected_body.len(),
        covered_from = marker.covered_from,
        compact_point = marker.compact_point,
        total_tokens = marker.total_tokens,
        derived_ids = marker.derived_host_ids.len(),
        runtime_note_chars = marker.to_runtime_note_text().len(),
        "LHC compact arm installed write-back from real CompactReceipt (rewrite path)"
    );

    // N3: any successful PreTurn/manual LHC install clears MidTurn
    // no-reduction hysteresis so a subsequent above-trigger MidTurn is not
    // suppressed by a stale margin band.
    slot.clear_mid_turn_hysteresis(
        /*attempt_id*/ if manual { "manual" } else { "preturn" },
        /*pressure*/ 0,
        /*outcome*/ "preturn_or_manual_install",
    );

    Ok(LhcCompactAttempt::Installed {
        body: expected_body,
        marker,
    })
}

/// Apply the degrade ladder's keep mask to the materialized rollout items so
/// the rewritten file rebuilds exactly the body the session installs (law 1).
///
/// Walks `items` in the order [`history_from_materialized_items`] reads them:
/// the newest `Compacted.replacement_history` (the bands) first, then the
/// post-boundary `ResponseItem` / `InterAgentCommunication` entries (the tail).
/// Positions the mask does not cover are kept — a shorter mask must never
/// silently truncate durable state.
pub(super) fn drop_materialized_items(items: &mut Vec<RolloutItem>, kept: &[bool]) {
    let boundary = items
        .iter()
        .rposition(|item| matches!(item, RolloutItem::Compacted(_)));
    let mut cursor = 0usize;
    if let Some(idx) = boundary
        && let RolloutItem::Compacted(compacted) = &mut items[idx]
        && let Some(history) = compacted.replacement_history.as_mut()
    {
        history.retain(|_| {
            let verdict = kept.get(cursor).copied().unwrap_or(true);
            cursor += 1;
            verdict
        });
    }
    let tail_start = boundary.map(|idx| idx + 1).unwrap_or(0);
    let mut position = 0usize;
    items.retain(|item| {
        let idx = position;
        position += 1;
        if idx < tail_start {
            return true;
        }
        match item {
            RolloutItem::ResponseItem(_) | RolloutItem::InterAgentCommunication(_) => {
                let verdict = kept.get(cursor).copied().unwrap_or(true);
                cursor += 1;
                verdict
            }
            _ => true,
        }
    });
}

/// Stamp host-assigned ids from `install_history` (bands + tail) onto the
/// materialized rollout sequence so the rewritten file and live memory match.
pub(super) fn patch_materialized_history_ids(
    items: &mut [RolloutItem],
    install_history: &[ResponseItem],
) {
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
                c.replacement_history = Some(bands.iter().cloned().map(Into::into).collect());
                past_boundary = true;
            }
            RolloutItem::ResponseItem(response_item) if past_boundary => {
                if let Some(src) = tail.get(tail_idx) {
                    *response_item = src.clone().into();
                    tail_idx += 1;
                }
            }
            _ => {}
        }
    }
}
