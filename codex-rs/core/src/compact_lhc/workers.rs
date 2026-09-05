//! Dedicated SDK runtime threads and their cancellation/join boundaries.
//! Worker timeouts and critical-section ordering are preserved from the caller.

use super::*;

/// Hop `run_mid_turn_compact_continuation` onto a current-thread runtime.
/// Keeps `!Send` LHC futures off the multi-thread session path.
///
/// Once mutation begins this is an **uninterruptible critical section**: the
/// join handle is always awaited. The operation timeout lives **inside** the
/// worker runtime so the future is dropped on that same thread at the deadline
/// and the thread exits; the caller then joins. Cancellation never detaches a
/// mutator — host apply is suppressed if the turn token cancelled during the
/// section.
/// Record host full-body validation on a dedicated thread (SDK futures are
/// `!Send`). Durable ok/failed acknowledgment for a protected-escalation
/// attempt; never rolls the core install back.
pub(super) async fn record_host_validation_on_thread(
    thread_id: String,
    root: Option<PathBuf>,
    attempt_id: String,
    ok: bool,
    reason: Option<String>,
) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawn = std::thread::Builder::new()
        .name(format!("lhc-midturn-hv-{attempt_id}"))
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
            let result = rt.block_on(codex_lhc_host::record_mid_turn_host_validation(
                &thread_id,
                root.as_deref(),
                &attempt_id,
                ok,
                reason,
            ));
            let _ = tx.send(result.map(|_| ()));
        });
    if let Err(err) = spawn {
        return Err(format!("spawn: {err}"));
    }
    match rx.await {
        Ok(result) => result,
        Err(_) => Err("host validation record thread dropped".into()),
    }
}

/// R12 (CX-S2): write-behind receipt for a rollout whose append recorder could
/// not reopen onto the freshly rewritten inode.
///
/// The compacted rollout is already durable and stays authoritative. This
/// records what the next open needs to reconcile appends that will not land in
/// it: the compacted rollout's identity/hash, the recorder frontier at failure,
/// and the canonical LHC capture frontier/event order (capture can keep
/// advancing while this process lives, even with a dead recorder handle).
///
/// Every failure in here is swallowed: a receipt that cannot be written costs
/// the next open its accounting, never the compact.
pub(super) async fn persist_reopen_failure_receipt(
    path: &std::path::Path,
    thread_id: &str,
    root: Option<PathBuf>,
    recorder_frontier_items: u64,
    reopen_error: &str,
) {
    let compacted_rollout = match codex_lhc_host::compacted_rollout_identity(path) {
        Ok(identity) => identity,
        Err(err) => {
            warn!(target: "codex_core::compact_lhc",
                %err,
                path = %path.display(),
                "LHC reopen-failure receipt: compacted rollout identity unreadable; \
                 recording the receipt without it"
            );
            codex_lhc_host::CompactedRolloutIdentity {
                sha256: String::new(),
                bytes: 0,
                items: recorder_frontier_items,
            }
        }
    };
    let capture_frontier = capture_frontier_on_thread(thread_id, root).await;
    if capture_frontier.is_none() {
        warn!(target: "codex_core::compact_lhc",
            thread_id,
            "LHC reopen-failure receipt: canonical capture frontier unavailable; \
             next open falls back to ordinary LHC-view reconstruction"
        );
    }
    let receipt = codex_lhc_host::RolloutReopenFailureReceipt {
        schema: codex_lhc_host::ROLLOUT_REOPEN_RECEIPT_SCHEMA.to_string(),
        written_at: chrono::Utc::now().to_rfc3339(),
        thread_id: thread_id.to_string(),
        rollout_path: path.display().to_string(),
        compacted_rollout,
        recorder_frontier_items,
        capture_frontier,
        reopen_error: reopen_error.to_string(),
    };
    match codex_lhc_host::write_rollout_reopen_failure_receipt(path, &receipt) {
        Ok(()) => info!(target: "codex_core::compact_lhc",
            path = %path.display(),
            recorder_frontier_items,
            "LHC reopen-failure receipt persisted; compacted rollout remains authoritative"
        ),
        Err(err) => warn!(target: "codex_core::compact_lhc",
            %err,
            path = %path.display(),
            "LHC reopen-failure receipt write failed; compact stands (receipts observe, never govern)"
        ),
    }
}

/// Read the canonical LHC capture frontier on a dedicated thread (SDK futures
/// are `!Send`). `None` whenever the archive cannot be read.
async fn capture_frontier_on_thread(
    thread_id: &str,
    root: Option<PathBuf>,
) -> Option<codex_lhc_host::CaptureFrontier> {
    let tid = thread_id.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawn = std::thread::Builder::new()
        .name(format!("lhc-capture-frontier-{tid}"))
        .spawn(move || {
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(codex_lhc_host::read_capture_frontier(&tid, root.as_deref()))
            }))
            .unwrap_or(None);
            let _ = tx.send(out);
        });
    let join = match spawn {
        Ok(join) => join,
        Err(err) => {
            warn!(target: "codex_core::compact_lhc", %err, "spawn lhc capture-frontier thread failed");
            return None;
        }
    };
    let frontier = rx.await.ok().flatten();
    let _ = tokio::task::spawn_blocking(move || join.join()).await;
    frontier
}

/// Name the attempt holding the durable LHC writer row, on a dedicated thread
/// (SDK inspection futures are `!Send`). `None` on any failure — a reclaim
/// probe never stops a compact.
pub(super) async fn writer_claim_owner_on_thread(
    thread_id: &str,
    root: Option<PathBuf>,
) -> Option<String> {
    let tid = thread_id.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawn = std::thread::Builder::new()
        .name(format!("lhc-midturn-claim-{tid}"))
        .spawn(move || {
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(codex_lhc_host::inspect_compact_continuation_writer_owner(
                    &tid,
                    root.as_deref(),
                ))
            }))
            .unwrap_or(None);
            let _ = tx.send(out);
        });
    let join = match spawn {
        Ok(join) => join,
        Err(err) => {
            warn!(target: "codex_core::compact_lhc", %err, "spawn lhc-midturn writer-claim probe thread failed");
            return None;
        }
    };
    let owner = rx.await.ok().flatten();
    let _ = tokio::task::spawn_blocking(move || join.join()).await;
    owner
}

/// Inspect durable MidTurn recovery identity on a dedicated thread (SDK
/// inspection futures are `!Send`).
pub(super) async fn inspect_mid_turn_recovery_on_thread(
    thread_id: &str,
    root: Option<PathBuf>,
) -> Result<Option<codex_lhc_host::MidTurnRecoveryIdentity>, String> {
    let tid = thread_id.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let join = std::thread::Builder::new()
        .name(format!("lhc-midturn-inspect-{tid}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("runtime: {e}"))?;
                rt.block_on(resolve_mid_turn_recovery_identity(&tid, root.as_deref()))
            }));
            let out = match result {
                Ok(inner) => inner,
                Err(_) => Err("lhc-midturn inspect thread panicked".into()),
            };
            let _ = tx.send(out);
        })
        .map_err(|e| format!("spawn lhc-midturn inspect thread: {e}"))?;
    let worker_out = rx.await;
    let join_result = tokio::task::spawn_blocking(move || join.join()).await;
    match join_result {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err("lhc-midturn inspect thread panicked during join".into()),
        Err(err) => return Err(format!("lhc-midturn inspect join task failed: {err}")),
    }
    match worker_out {
        Ok(r) => r,
        Err(_) => Err("lhc-midturn inspect channel closed".into()),
    }
}

/// Thread-name prefix for the MidTurn compact-continuation worker — the one
/// thread that mutates LHC SQLite for an attempt.
pub(crate) const MIDTURN_WORKER_THREAD_PREFIX: &str = "lhc-mt-";

pub(super) async fn run_mid_turn_on_thread(
    req: MidTurnCompactContinuationRequest,
    turn_cancel: &CancellationToken,
) -> Result<codex_lhc_host::MidTurnCompactContinuationOutcome, CompactWorkerError> {
    // If already cancelled before the critical section, refuse without spawn.
    if turn_cancel.is_cancelled() {
        return Err(CompactWorkerError::Cancelled(
            "lhc-midturn cancelled by turn abort before critical section".into(),
        ));
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    let attempt = req.attempt_id.clone();
    let worker_timeout = midturn_worker_timeout();
    // Short prefix on purpose: Linux truncates a thread's `comm` to 15 bytes,
    // so `lhc-midturn-{attempt}` reached /proc as `lhc-midturn-can` and every
    // MidTurn worker in the process looked alike. `lhc-mt-` leaves 8 bytes of
    // attempt id, which is what makes "is *this* attempt's mutator still
    // running?" answerable — from a debugger or from the no-detached-mutator
    // tests. Do not lengthen it.
    let join = std::thread::Builder::new()
        .name(format!("{MIDTURN_WORKER_THREAD_PREFIX}{attempt}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| CompactWorkerError::Worker(format!("runtime: {e}")))?;
                // Bound the operation future on this worker thread so a hung
                // `run_mid_turn_compact_continuation` is dropped at the deadline
                // and the thread can exit. The outer path always joins.
                rt.block_on(async move {
                    let op = async {
                        #[cfg(test)]
                        if let Some(stall) = midturn_worker_stall() {
                            tokio::time::sleep(stall).await;
                        }
                        run_mid_turn_compact_continuation(req).await
                    };
                    match tokio::time::timeout(worker_timeout, op).await {
                        Ok(inner) => inner.map_err(CompactWorkerError::Operation),
                        Err(_) => Err(CompactWorkerError::TimedOut(format!(
                            "lhc-midturn worker timed out after {}s (operation future dropped on worker)",
                            worker_timeout.as_secs_f64()
                        ))),
                    }
                })
            }));
            let out = match result {
                Ok(inner) => inner,
                Err(_) => Err(CompactWorkerError::Worker("lhc-midturn thread panicked".into())),
            };
            let _ = tx.send(out);
        })
        .map_err(|e| CompactWorkerError::Worker(format!("spawn lhc-midturn thread: {e}")))?;

    // Always join the worker. Never drop `join` while the mutator may still
    // run — R6 (CX-S2): cancellation no longer suppresses the host apply; the
    // joined worker's result is applied and the smaller body stands. Joining
    // here is what keeps the one-writer invariant: no detached thread may keep
    // mutating LHC SQLite after this function returns. The operation is
    // already bounded inside the worker, so join cannot hang forever on a
    // stalled compact-continuation future.
    let worker_out = rx.await;
    let join_result = tokio::task::spawn_blocking(move || join.join()).await;
    match join_result {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(CompactWorkerError::Worker(
                "lhc-midturn thread panicked during join".into(),
            ));
        }
        Err(err) => {
            return Err(CompactWorkerError::Worker(format!(
                "lhc-midturn join task failed: {err}"
            )));
        }
    }

    // R6 (CX-S2): the worker has finished and may have installed a view. A turn
    // that cancelled while it ran does not un-install that view, so its result
    // is returned and the caller applies it. Suppressing here is what left the
    // split state the next seam had to repair.
    match worker_out {
        Ok(r) => {
            if turn_cancel.is_cancelled() {
                warn!(target: "codex_core::compact_lhc",
                    "lhc-midturn turn cancelled during the critical section; returning the \
                     worker outcome so the installed view is applied"
                );
            }
            r
        }
        Err(_) => Err(CompactWorkerError::Worker(
            "lhc-midturn channel closed".into(),
        )),
    }
}

/// Why the parts hop produced no outcome. Only `Cancelled` blocks the next
/// provider request (the turn is ending); every `Failed` keeps the current
/// body and retries at a later eligible seam.
#[derive(Debug)]
pub(super) enum MidTurnPartsHopError {
    Cancelled(String),
    Failed(String),
}

/// Hop `run_mid_turn_parts_compact` onto a bounded current-thread runtime
/// (SDK futures are `!Send`). The SDK install is atomic, so a cancellation
/// during the section never leaves a split state; the worker is always joined.
pub(super) async fn run_mid_turn_parts_on_thread(
    req: MidTurnPartsRequest,
    attempt_id: &str,
    turn_cancel: &CancellationToken,
) -> Result<MidTurnPartsOutcome, MidTurnPartsHopError> {
    if turn_cancel.is_cancelled() {
        return Err(MidTurnPartsHopError::Cancelled(
            "lhc-midturn-parts cancelled by turn abort before critical section".into(),
        ));
    }
    run_mid_turn_parts_worker(req, attempt_id)
        .await
        .map_err(MidTurnPartsHopError::Failed)
}

async fn run_mid_turn_parts_worker(
    req: MidTurnPartsRequest,
    attempt_id: &str,
) -> Result<MidTurnPartsOutcome, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let worker_timeout = midturn_worker_timeout();
    // Same short prefix + attempt id as the legacy hop: "is *this* attempt's
    // mutator still running?" stays answerable from /proc and the tests.
    let join = std::thread::Builder::new()
        .name(format!("{MIDTURN_WORKER_THREAD_PREFIX}{attempt_id}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("runtime: {e}"))?;
                rt.block_on(async move {
                    let op = async {
                        #[cfg(test)]
                        if let Some(stall) = midturn_worker_stall() {
                            tokio::time::sleep(stall).await;
                        }
                        codex_lhc_host::run_mid_turn_parts_compact(req).await
                    };
                    match tokio::time::timeout(worker_timeout, op).await {
                        Ok(inner) => inner,
                        Err(_) => Err(format!(
                            "lhc-midturn-parts worker timed out after {}s (operation future dropped on worker)",
                            worker_timeout.as_secs_f64()
                        )),
                    }
                })
            }));
            let out = match result {
                Ok(inner) => inner,
                Err(_) => Err("lhc-midturn-parts thread panicked".into()),
            };
            let _ = tx.send(out);
        })
        .map_err(|e| format!("spawn lhc-midturn-parts thread: {e}"))?;
    let worker_out = rx.await;
    let join_result = tokio::task::spawn_blocking(move || join.join()).await;
    match join_result {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err("lhc-midturn-parts thread panicked during join".into()),
        Err(err) => return Err(format!("lhc-midturn-parts join task failed: {err}")),
    }
    match worker_out {
        Ok(r) => r,
        Err(_) => Err("lhc-midturn-parts channel closed".into()),
    }
}

/// Read materialize surfaces on a dedicated thread (LHC SDK futures are !Send).
pub(super) async fn read_materialize_surfaces_on_thread(
    thread_id: String,
    root: Option<PathBuf>,
    turn_cancel: &CancellationToken,
) -> Result<codex_lhc_host::MaterializeSurfaces, CompactWorkerError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let join = std::thread::Builder::new()
        .name(format!("lhc-materialize-{thread_id}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| CompactWorkerError::Worker(format!("runtime: {e}")))?;
                rt.block_on(async move {
                    read_materialize_surfaces(&thread_id, root.as_deref())
                        .await
                        .map_err(CompactWorkerError::Operation)
                })
            }))
            .unwrap_or_else(|payload| {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic in materialize surfaces thread".into());
                Err(CompactWorkerError::Worker(msg))
            });
            let _ = tx.send(result);
        })
        .map_err(|e| {
            CompactWorkerError::Worker(format!("spawn materialize surfaces thread: {e}"))
        })?;

    let result = tokio::select! {
        biased;
        () = turn_cancel.cancelled() => {
            // Detach: join will finish; we fail open.
            return Err(CompactWorkerError::Cancelled("turn cancelled while reading materialize surfaces".into()));
        }
        r = rx => r.map_err(|_| CompactWorkerError::Worker("materialize surfaces thread dropped".to_string()))?,
    };
    // Best-effort join so we don't leak threads on the happy path.
    let _ = join.join();
    result
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn produce_lhc_compact_on_thread(
    thread_id: String,
    root: Option<PathBuf>,
    host_items: Vec<ResponseItem>,
    import_missing: bool,
    callbacks: codex_lhc_host::InferenceCallbacks,
    cancel: Arc<AtomicBool>,
    session_derived: DerivedProvenance,
    percentages: LhcBandPercentages,
    turn_cancel: &CancellationToken,
) -> Result<LhcCompactResult, CompactWorkerError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cancel_thread = Arc::clone(&cancel);
    let join = std::thread::Builder::new()
        .name(format!("lhc-compact-{thread_id}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| CompactWorkerError::Worker(format!("runtime: {e}")))?;
                rt.block_on(async move {
                    produce_lhc_compact_with_provenance_and_percentages(
                        &thread_id,
                        root.as_deref(),
                        &host_items,
                        import_missing,
                        callbacks,
                        Some(cancel_thread),
                        &session_derived,
                        percentages,
                    )
                    .await
                    .map_err(CompactWorkerError::from)
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
                    Err(CompactWorkerError::Worker(msg))
                }
            };
            let _ = tx.send(out);
        })
        .map_err(|e| CompactWorkerError::Worker(format!("spawn lhc-compact thread: {e}")))?;

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
            warn!(target: "codex_core::compact_lhc", "LHC compact cancelled by turn abort; stopping produce (no native fallback)");
            return Err(CompactWorkerError::Cancelled("lhc-compact cancelled by turn abort".into()));
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
                        return Err(CompactWorkerError::Worker(
                            "lhc-compact thread panicked after success".into(),
                        ));
                    }
                }
                Ok(Err(e)) => {
                    warn!(target: "codex_core::compact_lhc", %e, "join spawn_blocking failed")
                }
                Err(_) => {
                    // Detach: drop JoinHandle (detaches thread) rather than block.
                    warn!(target: "codex_core::compact_lhc", "lhc-compact join timed out after success path; detaching thread");
                }
            }
            r
        }
        Ok(Err(_)) => {
            cancel.store(true, Ordering::SeqCst);
            // Detach rather than join — timeout must bound the caller (F4).
            drop(join);
            Err(CompactWorkerError::Worker(
                "lhc-compact thread dropped".into(),
            ))
        }
        Err(_) => {
            cancel.store(true, Ordering::SeqCst);
            // F4: do not await join on the timeout path — leak/detach the thread
            // so the turn can fail-open. A leaked thread beats a hung session.
            drop(join);
            warn!(target: "codex_core::compact_lhc",
                timeout_ms = thread_timeout.as_millis() as u64,
                "lhc-compact timed out; detaching worker thread (turn continues on its current body)"
            );
            Err(CompactWorkerError::TimedOut(format!(
                "lhc-compact timed out after {}ms",
                thread_timeout.as_millis()
            )))
        }
    }
}

/// Total wall-clock budget for committing the archive marker note, retries
/// included. Brief on purpose: the durable compact record is the rollout
/// `Compacted` item (already fsynced by the rewrite before this runs); the
/// archive note duplicates the same serialized marker as an observation, so
/// a wedged archive must not hold the turn.
const MARKER_COMMIT_BUDGET: Duration = Duration::from_secs(2);

/// Commit the archive marker note, retrying transient failures inside
/// [`MARKER_COMMIT_BUDGET`].
///
/// The archive open/submit can fail for reasons that say nothing about whether
/// the thread is writable — a busy registry, a concurrent opener. A brief
/// retry absorbs that contention. Retrying is safe: the marker carries an
/// idempotency key, so a retry after a submit that actually landed is a no-op.
///
/// Losing the note is recoverable, not silent data loss: the same marker
/// payload lives in the rollout `Compacted` record, which
/// `seed_last_lhc_durable_from_rollout` reads at next open and
/// `reseed_slot_from_durable_session` merges into the slot's derived
/// provenance — the same sets `DerivedProvenance::from_session_and_archive`
/// consults, so re-ingest prevention holds without the archive copy.
///
/// The budget is a ceiling, not a target — it is spent only when the archive is
/// genuinely wedged, and it never lengthens the successful path.
pub(super) async fn commit_marker_with_retry(
    thread_id: String,
    root: Option<PathBuf>,
    marker: &CompactMarker,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + MARKER_COMMIT_BUDGET;
    let mut backoff = Duration::from_millis(50);
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let err = match commit_marker_on_thread(
            thread_id.clone(),
            root.clone(),
            marker.clone(),
            remaining,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining <= backoff {
            return Err(format!("{err} (after {attempt} attempt(s))"));
        }
        warn!(target: "codex_core::compact_lhc",
            %err,
            attempt,
            "LHC compact archive marker commit failed; retrying within the commit budget"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 4).min(Duration::from_secs(2));
    }
}

async fn commit_marker_on_thread(
    thread_id: String,
    root: Option<PathBuf>,
    marker: CompactMarker,
    timeout: Duration,
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
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("marker thread dropped".into()),
        Err(_) => Err("marker commit timed out".into()),
    }
}
