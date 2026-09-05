//! LHC compact arm — real `lhc.compact` body + write-back (Chunk 2b redo).
//!
//! Fork policy (strict): manual `/compact` and every automatic compact caller
//! share one LHC-only outcome surface. Native TokenBudget / remote / local
//! compact is not reachable. Recoverable degraded conditions continue via
//! fallback bands / full-fidelity residue; only genuine hard stops fail.
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

#[path = "compact_lhc_worker_error.rs"]
mod worker_error;
use worker_error::CompactWorkerError;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_analytics::CompactionPhase;
use codex_analytics::CompactionTrigger;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_lhc_host::CaptureState;
use codex_lhc_host::CompactBoundaryMeta;
use codex_lhc_host::CompactMarker;
use codex_lhc_host::DEFAULT_LOWER_TARGET_TOKENS;
use codex_lhc_host::DerivedProvenance;
use codex_lhc_host::InferenceCallbacks;
use codex_lhc_host::LhcBandPercentages;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::LhcCompactResult;
use codex_lhc_host::MaterializeInput;
use codex_lhc_host::MidTurnCompactContinuationRequest;
use codex_lhc_host::MidTurnPartsOutcome;
use codex_lhc_host::MidTurnPartsRequest;
use codex_lhc_host::WorkContinuation;
use codex_lhc_host::WriterClaim;
use codex_lhc_host::atomic_rewrite_rollout_as_generation;
use codex_lhc_host::commit_compact_marker;
use codex_lhc_host::compact_opts_with_band_percentages;
use codex_lhc_host::content_identity_digest;
use codex_lhc_host::estimate_response_items_tokens;
use codex_lhc_host::history_from_materialized_items;
use codex_lhc_host::inert_non_deriving_inference_callbacks;
use codex_lhc_host::item_stable_id;
use codex_lhc_host::materialize_rollout;
use codex_lhc_host::missing_provider_usage_authority;
use codex_lhc_host::model_context_token_estimate_from_rollout_items;
use codex_lhc_host::next_request_pressure;
use codex_lhc_host::parse_rollout_items;
use codex_lhc_host::produce_lhc_compact_with_provenance_and_percentages;
use codex_lhc_host::read_materialize_surfaces;
use codex_lhc_host::resolve_mid_turn_recovery_identity;
use codex_lhc_host::run_mid_turn_compact_continuation;
use codex_lhc_host::token_usage_to_provider_usage_authority;
use codex_lhc_host::work_continuation_for_mid_turn;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TokenUsage;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::compact::InitialContextInjection;
use crate::compact::build_compaction_initial_context;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::session::context_window::context_window_token_status;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

#[path = "compact_lhc/preparation.rs"]
mod preparation;
// Preserve the original crate-visible constant paths, including non-test builds.
#[allow(unused_imports)]
pub(crate) use preparation::STRICT_COMPACT_READINESS_BOUND;
use preparation::configured_band_percentages;
pub(crate) use preparation::seed_lhc_derivation_callbacks;
use preparation::select_production_inference_callbacks;
pub(crate) use preparation::try_run_lhc_compact_arm_with_callbacks_and_cancel;
#[path = "compact_lhc/workers.rs"]
mod workers;
#[allow(unused_imports)]
pub(crate) use workers::MIDTURN_WORKER_THREAD_PREFIX;
use workers::MidTurnPartsHopError;
use workers::commit_marker_with_retry;
use workers::inspect_mid_turn_recovery_on_thread;
use workers::persist_reopen_failure_receipt;
use workers::produce_lhc_compact_on_thread;
use workers::read_materialize_surfaces_on_thread;
use workers::record_host_validation_on_thread;
use workers::run_mid_turn_on_thread;
use workers::run_mid_turn_parts_on_thread;
use workers::writer_claim_owner_on_thread;

#[path = "compact_lhc/installation.rs"]
mod installation;
#[cfg(test)]
use installation::drop_materialized_items;
use installation::install_lhc_compact_rewrite;
#[cfg(test)]
use installation::patch_materialized_history_ids;

const COMPACT_THREAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Best-effort capture flush before produce. Must stay well below the produce
/// bound so a wedged worker cannot hide the compact deadline.
#[cfg(not(test))]
const COMPACT_FLUSH_BOUND: Duration = Duration::from_secs(5);
#[cfg(test)]
const COMPACT_FLUSH_BOUND: Duration = Duration::from_millis(200);
/// MidTurn tests run alongside background capture work; keep their bound long
/// enough for healthy workers while still proving a blocked worker cannot hang.
#[cfg(not(test))]
const MIDTURN_COMPACT_FLUSH_BOUND: Duration = Duration::from_secs(5);
#[cfg(test)]
const MIDTURN_COMPACT_FLUSH_BOUND: Duration = Duration::from_secs(2);

/// Process-wide MidTurn worker timeout override used only by offline tests.
/// `None` restores the production 120s bound.
///
/// Guarded by [`MIDTURN_WORKER_OVERRIDE_LOCK`] so plain-parallel test runs
/// cannot race timeout/stall injections against each other.
#[cfg(test)]
static MIDTURN_WORKER_TIMEOUT_OVERRIDE: std::sync::Mutex<Option<Duration>> =
    std::sync::Mutex::new(None);

/// Process-wide MidTurn worker stall injected at the start of the timed
/// operation future (tests only). Used to prove the timeout drops the future
/// on the worker thread without detaching.
#[cfg(test)]
static MIDTURN_WORKER_STALL_OVERRIDE: std::sync::Mutex<Option<Duration>> =
    std::sync::Mutex::new(None);

/// Serializes tests that mutate the process-wide MidTurn worker knobs.
#[cfg(test)]
static MIDTURN_WORKER_OVERRIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test-only RAII guard: holds the process-wide MidTurn worker override lock
/// and clears both overrides on drop so plain-parallel runs stay isolated even
/// when a test panics mid-body.
#[cfg(test)]
pub(crate) struct MidturnWorkerOverrideGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl MidturnWorkerOverrideGuard {
    pub(crate) fn acquire() -> Self {
        let lock = MIDTURN_WORKER_OVERRIDE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Start from a clean slate for this holder.
        *MIDTURN_WORKER_TIMEOUT_OVERRIDE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *MIDTURN_WORKER_STALL_OVERRIDE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Self { _lock: lock }
    }

    pub(crate) fn set_timeout(&self, timeout: Option<Duration>) {
        *MIDTURN_WORKER_TIMEOUT_OVERRIDE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = timeout;
    }

    pub(crate) fn set_stall(&self, stall: Option<Duration>) {
        *MIDTURN_WORKER_STALL_OVERRIDE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = stall;
    }
}

#[cfg(test)]
impl Drop for MidturnWorkerOverrideGuard {
    fn drop(&mut self) {
        *MIDTURN_WORKER_TIMEOUT_OVERRIDE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *MIDTURN_WORKER_STALL_OVERRIDE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Test-only: hold the process-wide MidTurn worker override lock.
#[cfg(test)]
pub(crate) fn midturn_worker_override_guard() -> MidturnWorkerOverrideGuard {
    MidturnWorkerOverrideGuard::acquire()
}

/// Test-only: bound the MidTurn worker timeout without waiting 120s.
/// Prefer [`MidturnWorkerOverrideGuard::set_timeout`] so drop cleans up.
#[cfg(test)]
pub(crate) fn set_midturn_worker_timeout_override(timeout: Option<Duration>) {
    *MIDTURN_WORKER_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = timeout;
}

/// Test-only: stall the MidTurn worker operation future for `stall` before the
/// certified runtime runs.
#[cfg(test)]
pub(crate) fn set_midturn_worker_stall_override(stall: Option<Duration>) {
    *MIDTURN_WORKER_STALL_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = stall;
}

fn midturn_worker_timeout() -> Duration {
    #[cfg(test)]
    {
        if let Some(t) = *MIDTURN_WORKER_TIMEOUT_OVERRIDE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return t;
        }
    }
    COMPACT_THREAD_TIMEOUT
}

#[cfg(test)]
fn midturn_worker_stall() -> Option<Duration> {
    *MIDTURN_WORKER_STALL_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Installed body is the load-bearing result; boxing breaks law-1 callers.
pub(crate) enum LhcCompactAttempt {
    Installed {
        /// Served body installed on the host (law-1 reference).
        #[allow(dead_code)]
        body: Vec<ResponseItem>,
        /// Receipt-backed marker (committed after write-back).
        #[allow(dead_code)]
        marker: CompactMarker,
    },
    /// Hard stop: preserve current history. Never permission for native compact.
    Failed { reason: String },
    /// The rollout swap was interrupted and reconciliation could not establish
    /// one authoritative active generation on disk (see
    /// `codex_lhc_host::reconcile_interrupted_swap`). Sampling must not
    /// continue on this rollout: the current in-memory body is preserved and
    /// the exact on-disk state is reported. Never permission for native
    /// compact or compact-continuation.
    RolloutUnreconciled { reason: String },
    /// The LHC arm could not run. Strict dispatch treats this as a hard stop;
    /// it is never permission for native compact.
    Unavailable { reason: String },
    /// Turn cancellation. Not permission for native compact.
    Cancelled { reason: String },
    /// MidTurn compact-continuation explicitly skipped (transport retry, below
    /// trigger, unsettled stream). No host mutation; next provider request
    /// allowed.
    MidTurnSkipped { reason: String },
    /// No compact was produced, but the session is not stranded: the turn
    /// continues on its current body and compact retries at the next eligible
    /// seam. Never permission for native compact.
    ContinuedWithoutCompact { reason: String },
    /// MidTurn compact-continuation refused or failed. Native compact remains
    /// unreachable; the flag controls only whether sampling may continue.
    MidTurnBlocked {
        reason: String,
        next_provider_request_allowed: bool,
    },
}

/// Shared strict dispatch for manual `CompactTask` and every automatic path.
/// Native TokenBudget / remote / local compact is never reachable.
pub(crate) async fn run_strict_lhc_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    initial_context_injection: InitialContextInjection,
    manual: bool,
    phase: CompactionPhase,
    mid_turn: Option<MidTurnSeamFacts>,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let trigger = if manual {
        CompactionTrigger::Manual
    } else {
        CompactionTrigger::Auto
    };
    // R15 (CX-S1): PreCompact hooks are notification only. Compact is the
    // recovery mechanism, so an external hook does not get to veto one the fork
    // has already decided to run; a stop request is recorded and ignored.
    // PostCompact keeps its notification role for hooks that need to observe.
    if let PreCompactHookOutcome::Stopped = run_pre_compact_hooks(sess, turn_context, trigger).await
    {
        warn!(
            manual,
            "PreCompact hook requested stop; LHC compact continues (hook veto removed)"
        );
    }

    match try_run_lhc_compact_arm(
        sess,
        turn_context.as_ref(),
        initial_context_injection,
        manual,
        phase,
        mid_turn,
        cancellation_token,
    )
    .await?
    {
        LhcCompactAttempt::Installed { .. } => {
            let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
            sess.emit_turn_item_started(turn_context.as_ref(), &compaction_item)
                .await;
            sess.emit_turn_item_completed(turn_context.as_ref(), compaction_item)
                .await;
            crate::tasks::emit_compact_metric(&sess.services.session_telemetry, "lhc", manual);
            match run_post_compact_hooks(sess, turn_context, trigger).await {
                PostCompactHookOutcome::Continue => Ok(()),
                PostCompactHookOutcome::Stopped => {
                    info!(
                        manual,
                        "PostCompact hook stopped after LHC compact; treating as TurnAborted"
                    );
                    Err(CodexErr::TurnAborted)
                }
            }
        }
        LhcCompactAttempt::Cancelled { reason } => {
            debug!(%reason, manual, "LHC compact cancelled; not falling back to native");
            Err(CodexErr::TurnAborted)
        }
        LhcCompactAttempt::Failed { reason } | LhcCompactAttempt::Unavailable { reason } => {
            error!(%reason, manual, "LHC compact hard failure; preserving history (no native compact)");
            Err(CodexErr::UnsupportedOperation(format!(
                "LHC compact failed: {reason}"
            )))
        }
        LhcCompactAttempt::RolloutUnreconciled { reason } => {
            error!(
                %reason,
                manual,
                "LHC rollout swap left no single authoritative generation; denying further \
                 sampling on this rollout (history preserved, no native compact)"
            );
            Err(CodexErr::UnsupportedOperation(format!(
                "LHC rollout swap unreconciled: {reason}"
            )))
        }
        LhcCompactAttempt::MidTurnSkipped { reason } => {
            info!(%reason, "LHC MidTurn compact-continuation skipped; continuing without native compact");
            Ok(())
        }
        LhcCompactAttempt::ContinuedWithoutCompact { reason } => {
            warn!(
                %reason,
                manual,
                "LHC compact did not produce a body; turn continues on its current body (no native compact)"
            );
            Ok(())
        }
        LhcCompactAttempt::MidTurnBlocked {
            reason,
            next_provider_request_allowed,
        } => {
            if next_provider_request_allowed {
                warn!(%reason, "LHC MidTurn compact-continuation blocked mutation; continuing without native compact");
                Ok(())
            } else {
                error!(
                    %reason,
                    "LHC MidTurn compact-continuation blocked next provider request; aborting turn (no native compact)"
                );
                Err(CodexErr::TurnAborted)
            }
        }
    }
}

fn cancelled_attempt(reason: impl Into<String>) -> LhcCompactAttempt {
    LhcCompactAttempt::Cancelled {
        reason: reason.into(),
    }
}

fn failed_attempt(reason: impl Into<String>) -> LhcCompactAttempt {
    LhcCompactAttempt::Failed {
        reason: reason.into(),
    }
}

/// R19 (CX-S3): body assembly produced nothing installable.
///
/// This is never a hard failure. The session keeps the body it already holds,
/// the turn continues on it, and compact retries at the next seam — the same
/// disposition as any other "no body this time" outcome. Failing here would
/// strand a session over an empty derivation, which is the one cost no
/// bookkeeping is worth.
fn kept_prior_body_attempt(reason: impl Into<String>) -> LhcCompactAttempt {
    LhcCompactAttempt::ContinuedWithoutCompact {
        reason: reason.into(),
    }
}

// LHC-HOOK: LHC compact arm entry (manual + auto ladders).

/// MidTurn inputs collected at the settled post-sampling seam.
#[derive(Debug, Clone)]
pub(crate) struct MidTurnSeamFacts {
    /// Provider response id (stable attempt identity) when available.
    pub attempt_id: String,
    /// Token usage from the completed provider response (not a later aggregate).
    pub response_token_usage: Option<TokenUsage>,
    /// Response-scoped **client-executed** tool call IDs from the just-completed
    /// sampling response (FunctionCall / CustomToolCall / LocalShellCall /
    /// ToolSearchCall). Analytics-only server-side items are excluded.
    pub response_tool_call_ids: Vec<String>,
    /// Total continuation intent: model tool follow-up **or** queued
    /// steering/mailbox/hook work that plans a next provider request.
    pub total_needs_follow_up: bool,
    /// Input-queue epoch at rollover decision time (not history_version).
    pub input_epoch_at_decision: i64,
    /// True only while a transport retry is in flight (never compact then).
    pub inside_transport_retry: bool,
    /// True only when the provider stream settled on `ResponseEvent::Completed`.
    /// Mailbox-preempted / abandoned streams must pass false.
    pub model_response_complete: bool,
}

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
    live_identity: Option<codex_lhc_host::ModelIdentity>,
) {
    // 0.153.3 sync: upstream compresses cold (7-day idle) rollouts to
    // `.jsonl.zst`. Classification reads the plain path, so restore the plain
    // representation first (upstream's own resume/append path does the same
    // before it references the file); otherwise a compressed-but-intact file
    // would classify as MISSING and be regenerated from the LHC record.
    if !rollout_path.exists()
        && codex_rollout::existing_rollout_path(rollout_path)
            .await
            .is_some()
    {
        match codex_rollout::materialize_rollout_for_reference(rollout_path).await {
            Ok(plain) => info!(
                path = %plain.display(),
                thread_id,
                "LHC startup reconciliation: materialized compressed rollout before classify"
            ),
            Err(err) => warn!(
                %err,
                path = %rollout_path.display(),
                thread_id,
                "LHC startup reconciliation: could not materialize compressed rollout; \
                 classification proceeds on the plain path"
            ),
        }
    }
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
                live_identity,
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
// override. If the real client cannot be resolved, compact continues with the
// inert non-deriving seam (existing bands / residue) — never canned text and
// never native compact.
#[tracing::instrument(level = "info", skip_all, fields(manual = manual, phase = ?phase))]
pub(crate) async fn try_run_lhc_compact_arm(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    initial_context_injection: InitialContextInjection,
    manual: bool,
    phase: CompactionPhase,
    mid_turn: Option<MidTurnSeamFacts>,
    cancellation_token: &CancellationToken,
) -> CodexResult<LhcCompactAttempt> {
    // R17 (CX-S1): `MidTurnSeamFacts` are constructed unconditionally by the
    // MidTurn caller (`session/turn.rs`), so absent facts are not a runtime
    // condition the system can produce. The old missing-facts guard blocked the
    // next provider request for that impossible state; it is gone. A MidTurn
    // dispatch without facts declines into the ordinary settled-seam compact
    // rather than stopping anything.
    if matches!(phase, CompactionPhase::MidTurn)
        && let Some(mid) = mid_turn
    {
        // Box to keep rustc query depth under the limit when nested under run_turn.
        return Box::pin(try_run_mid_turn_arm(
            sess,
            turn_context,
            initial_context_injection,
            mid,
            cancellation_token,
        ))
        .await;
    }

    let callbacks = match select_production_inference_callbacks(sess.as_ref()).await {
        Ok(c) => c,
        Err(reason) => {
            warn!(
                %reason,
                manual,
                "LHC derivation model unavailable; compact continues with inert non-deriving seam"
            );
            inert_non_deriving_inference_callbacks()
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
///
/// Non-MidTurn path only (legacy band compact / PreTurn / Standalone).
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

/// Turn parts MidTurn arm (Story 5): at the settled post-sampling seam invoke
/// the certified SDK `mid_turn_compact` — the ordinary bounded compact that
/// splits the active turn into parts (or settles/compacts) with no synthetic
/// boundary or continuation turn. The single AC-7.3 amendment is typed-only:
/// **only** a typed `ForcedBoundaryThread` refusal routes an already-classified
/// thread to the legacy compact-continuation path. Every other refusal, storage
/// error, flush failure, or missing active turn preserves the current body and
/// retries at a later eligible seam — it never falls open to native compaction
/// and never asserts a false seam fact.
async fn try_run_mid_turn_arm(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    initial_context_injection: InitialContextInjection,
    mid: MidTurnSeamFacts,
    cancellation_token: &CancellationToken,
) -> CodexResult<LhcCompactAttempt> {
    if cancellation_token.is_cancelled() {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason: "turn cancelled before MidTurn parts compact".into(),
            next_provider_request_allowed: false,
        });
    }
    if !sess.enabled(Feature::LhcCapture) {
        return Ok(failed_attempt(
            "Feature::LhcCapture off; native compact is disabled in this fork",
        ));
    }
    let Some(slot) = sess.services.thread_extension_data.get::<LhcCaptureSlot>() else {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason:
                "no LhcCaptureSlot at MidTurn; next provider request continues on the existing body"
                    .into(),
            next_provider_request_allowed: true,
        });
    };
    let Some(handle) = slot.get() else {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason: "LHC capture handle not ready at MidTurn; incomplete facts, no mutation".into(),
            next_provider_request_allowed: true,
        });
    };
    // Transport retries inside one outer sampling cycle never compact.
    if mid.inside_transport_retry {
        return Ok(LhcCompactAttempt::MidTurnSkipped {
            reason: "inside transport retry; stable view, no MidTurn compact".into(),
        });
    }
    // AC-7.4: the host asserts `captureFlushed` only on a *successful* flush.
    // A flush that does not complete is not a settled seam — keep the current
    // body and retry, never assert a false fact.
    if !handle.flush_within(MIDTURN_COMPACT_FLUSH_BOUND).await {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason: "capture flush did not complete at MidTurn seam; not a settled seam, retrying at the next one"
                .into(),
            next_provider_request_allowed: true,
        });
    }
    // An unsettled stream (mailbox preempt / abandoned) is not a settled seam.
    if !mid.model_response_complete {
        return Ok(LhcCompactAttempt::MidTurnSkipped {
            reason: "model response incomplete (preempted/abandoned stream); no MidTurn compact"
                .into(),
        });
    }

    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);

    // AC-7.4 (host side): the durable active turn must be exactly this Codex
    // turn. `LhcTurnId` is the host identity; capture bound it to the SDK's
    // durable turn name when that turn's opening prompt was committed. No
    // current identity, or no binding yet (the open turn is not committed),
    // keeps the current body without invoking compact; the adapter then
    // compares the bound id with `host_metadata.active_turn.turn_id` exactly.
    let Some(host_turn_id) = turn_context
        .extension_data
        .get::<codex_lhc_host::LhcTurnId>()
        .map(|id| id.0.clone())
    else {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason: "no current host turn identity (LhcTurnId) at the MidTurn seam; keeping current body"
                .into(),
            next_provider_request_allowed: true,
        });
    };
    let Some(active_turn_id) = handle.durable_turn_id(&host_turn_id) else {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason: format!(
                "durable turn for host turn {host_turn_id} not yet bound at the MidTurn seam; \
                 keeping current body, retrying at a later seam"
            ),
            next_provider_request_allowed: true,
        });
    };

    // Band percentages / lower target for the bounded walk (test override when set).
    let compact = {
        let configured =
            compact_opts_with_band_percentages(configured_band_percentages(turn_context));
        #[cfg(any(test, feature = "test-util"))]
        {
            slot.mid_turn_test_compact().or(Some(configured))
        }
        #[cfg(not(any(test, feature = "test-util")))]
        {
            Some(configured)
        }
    };

    let req = MidTurnPartsRequest {
        thread_id: thread_id.clone(),
        root: root.clone(),
        active_turn_id,
        compact,
        created_at: None,
    };
    let outcome = match run_mid_turn_parts_on_thread(req, &mid.attempt_id, cancellation_token).await
    {
        Ok(o) => o,
        Err(MidTurnPartsHopError::Cancelled(reason)) => {
            // The turn is ending; nothing was invoked and no request follows.
            return Ok(LhcCompactAttempt::MidTurnBlocked {
                reason,
                next_provider_request_allowed: false,
            });
        }
        Err(MidTurnPartsHopError::Failed(err)) => {
            // Storage, worker, timeout, or panic: nothing durable changed (the
            // SDK install is atomic). Keep the current body and retry at a
            // later eligible seam — never a fall-open to native compaction and
            // never the compact-continuation path.
            warn!(%err, "LHC MidTurn parts compact failed; next provider request continues on the existing body");
            return Ok(LhcCompactAttempt::MidTurnBlocked {
                reason: err,
                next_provider_request_allowed: true,
            });
        }
    };

    match outcome {
        MidTurnPartsOutcome::Installed(receipt) => {
            // Reuse the existing atomic materialize / rollout-rewrite / in-memory
            // install path. The SDK already installed the serving view (parts or
            // whole); no continuation marker, no forced boundary. CompactMarker
            // here is ordinary fork bookkeeping (the durable Compacted record).
            let marker =
                CompactMarker::from_receipt(&receipt, &thread_id, /*body*/ &[], "midturn");
            let (_initial_context, world_state_baseline) =
                build_compaction_initial_context(sess.as_ref(), &initial_context_injection).await;
            let reference_context_item = match &initial_context_injection {
                InitialContextInjection::DoNotInject => None,
                InitialContextInjection::BeforeLastUserMessage { .. } => {
                    Some(turn_context.to_turn_context_item())
                }
            };
            // The SDK install already happened; the host rewrite is the atomic
            // completion and runs under a token that is never cancelled so a
            // cancellation race cannot leave a split state.
            let apply_token = CancellationToken::new();
            let applied = Box::pin(install_lhc_compact_rewrite(
                sess,
                turn_context,
                &slot,
                thread_id,
                root,
                marker,
                world_state_baseline,
                reference_context_item,
                /*manual*/ false,
                /*host_validation*/ None,
                &apply_token,
            ))
            .await;
            finish_parts_host_apply(applied)
        }
        MidTurnPartsOutcome::ForcedBoundaryThread => {
            // AC-7.3 (typed-only): this thread already took the forced-boundary
            // path before the migration. Route it — and only it — through the
            // legacy compact-continuation mechanism so it behaves exactly as
            // before. A thread that ever served parts never reaches here (the
            // SDK would not return this code), so once parts activated the old
            // path cannot run.
            Box::pin(run_mid_turn_forced_boundary_continuation(
                sess,
                turn_context,
                initial_context_injection,
                mid,
                cancellation_token,
            ))
            .await
        }
        MidTurnPartsOutcome::ActiveTurnMismatch { expected, durable } => {
            // AC-7.4: the durable active turn is not this Codex turn. Compact
            // was not invoked; keep the current body, retry at a later seam.
            Ok(LhcCompactAttempt::MidTurnBlocked {
                reason: format!(
                    "durable active turn {} is not the current host turn's bound turn {expected}; \
                     keeping current body",
                    durable.as_deref().unwrap_or("<none>")
                ),
                next_provider_request_allowed: true,
            })
        }
        MidTurnPartsOutcome::Refused { code, reason } => {
            // Typed refusal at a truthful settled seam (not ForcedBoundaryThread):
            // no mutation happened. Keep the current body; retry at a later seam.
            // Never a license for native compaction.
            Ok(LhcCompactAttempt::MidTurnBlocked {
                reason: format!("mid-turn parts refused code={code} reason={reason}"),
                next_provider_request_allowed: true,
            })
        }
    }
}

/// Turn parts (Story 5): disposition of the host apply that follows an SDK
/// parts install. The SDK has installed the serving view; the host's
/// materialize / rollout-rewrite / in-memory install is its atomic completion.
/// When that completion does not happen, the host still holds the body it
/// had — nothing is torn (a failed rewrite leaves the old rollout
/// authoritative) — so the only correct disposition is *retry later*: keep the
/// current body, allow the next provider request, and let the next eligible
/// seam run the parts compact again, which re-materializes against the
/// standing SDK view. It is never a hard stop (strict dispatch must not end
/// the turn over a host apply that preserved the body), never native
/// compaction, and never compact-continuation. Cancellation / abort remains a
/// deny case: the apply token is never cancelled, and an interrupted /
/// aborted host error is passed through unchanged. The only other deny is
/// `RolloutUnreconciled`: the rewrite was interrupted between its rename
/// edges and `reconcile_interrupted_swap` could not establish one
/// authoritative on-disk generation — sampling on a rollout whose authority
/// is unknown would split disk from served memory. Every state it *could*
/// resolve is handled before this function sees it: old-active /
/// restored-old arrive as `Failed` (retry later), and new-active arrives as
/// `Installed` because the host completed its in-memory / window install
/// against the generation that is actually on disk.
fn finish_parts_host_apply(
    applied: CodexResult<LhcCompactAttempt>,
) -> CodexResult<LhcCompactAttempt> {
    let reason = match applied {
        Ok(attempt @ LhcCompactAttempt::Installed { .. })
        | Ok(attempt @ LhcCompactAttempt::Cancelled { .. })
        // An interrupted swap that reconciliation could not resolve to one
        // authoritative generation denies sampling (see the variant docs);
        // every reconciled state below is either installed or retry-later.
        | Ok(attempt @ LhcCompactAttempt::RolloutUnreconciled { .. }) => return Ok(attempt),
        Ok(LhcCompactAttempt::Failed { reason })
        | Ok(LhcCompactAttempt::Unavailable { reason })
        | Ok(LhcCompactAttempt::MidTurnSkipped { reason })
        | Ok(LhcCompactAttempt::ContinuedWithoutCompact { reason })
        | Ok(LhcCompactAttempt::MidTurnBlocked { reason, .. }) => reason,
        Err(err)
            if matches!(
                err.details(),
                CodexErrorDetails::Interrupted | CodexErrorDetails::TurnAborted
            ) =>
        {
            return Err(err);
        }
        Err(err) => err.to_string(),
    };
    warn!(
        %reason,
        "LHC mid-turn parts: SDK view installed but the host apply did not complete; \
         keeping current body, retry at a later seam (no native compact, no continuation)"
    );
    Ok(LhcCompactAttempt::MidTurnBlocked {
        reason: format!(
            "mid-turn parts view installed but host apply did not complete ({reason}); \
             keeping current body, retry at a later seam"
        ),
        next_provider_request_allowed: true,
    })
}

/// Legacy compact-continuation MidTurn path (LIM-63B). Reached in Story 5 only
/// for a thread the SDK typed as `ForcedBoundaryThread` (AC-7.3 coexistence).
/// LHC is the single writer; never silently falls open to native.
async fn run_mid_turn_forced_boundary_continuation(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    initial_context_injection: InitialContextInjection,
    mid: MidTurnSeamFacts,
    cancellation_token: &CancellationToken,
) -> CodexResult<LhcCompactAttempt> {
    if cancellation_token.is_cancelled() {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason: "turn cancelled before MidTurn compact-continuation".into(),
            next_provider_request_allowed: false,
        });
    }
    if !sess.enabled(Feature::LhcCapture) {
        return Ok(failed_attempt(
            "Feature::LhcCapture off; native compact is disabled in this fork",
        ));
    }

    let Some(slot) = sess.services.thread_extension_data.get::<LhcCaptureSlot>() else {
        // R16 (CX-S1): the slot is structural and its absence is a transient
        // startup condition, never a reason to strand the turn. Sampling
        // continues on the existing body; compact retries at the next seam.
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason:
                "no LhcCaptureSlot at MidTurn; next provider request continues on the existing body"
                    .into(),
            next_provider_request_allowed: true,
        });
    };
    let Some(handle) = slot.get() else {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason: "LHC capture handle not ready at MidTurn; incomplete facts, no mutation".into(),
            next_provider_request_allowed: true,
        });
    };
    // R2 (CX-S1): capture feeds derivation quality, not compact capability —
    // the SDK compacts the LHC thread, not the capture buffer. A degraded
    // capture is a reason to compact (the session is big), never a reason to
    // strand it. Detect, warn, continue — the ordinary path (G33) already does.
    if handle.is_degraded() {
        warn!(
            "LHC capture degraded at MidTurn; compact-continuation continues (thread is the source)"
        );
    }

    if mid.inside_transport_retry {
        return Ok(LhcCompactAttempt::MidTurnSkipped {
            reason: "inside transport retry; stable view, no MidTurn compact".into(),
        });
    }

    // R2 (CX-S1): bounded flush before the decision so the seam sees as much
    // captured content as the worker can produce. A wedged or slow capture
    // worker is warned about and compact continues — the ordinary path (G34)
    // behaves the same way.
    if !handle.flush_within(MIDTURN_COMPACT_FLUSH_BOUND).await {
        warn!(
            timeout_ms = MIDTURN_COMPACT_FLUSH_BOUND.as_millis() as u64,
            "LHC capture flush did not complete in time at MidTurn; compact-continuation continues"
        );
    }
    if handle.is_degraded() {
        warn!("LHC capture degraded after MidTurn flush; compact-continuation continues");
    }

    // R1 (CX-S1): the input-queue epoch is a diagnostic, not an authority.
    // Settled history is not invalidated by input that arrives after the
    // rollover decision — that input belongs to the next turn. The
    // decision-to-apply veto is gone; drift is logged and compact proceeds.
    let input_epoch_at_apply = i64::try_from(sess.input_queue.input_epoch()).unwrap_or(i64::MAX);
    if mid.input_epoch_at_decision != input_epoch_at_apply {
        info!(
            decision = mid.input_epoch_at_decision,
            apply = input_epoch_at_apply,
            "LHC MidTurn input epoch changed since the rollover decision; compact continues"
        );
    }

    // Unsettled stream (mailbox preempt / abandoned): certified runtime skips
    // without mutation. Do not assert a completed response or stale usage.
    if !mid.model_response_complete {
        return Ok(LhcCompactAttempt::MidTurnSkipped {
            reason: "model response incomplete (preempted/abandoned stream); no MidTurn compact"
                .into(),
        });
    }

    let token_status = context_window_token_status(sess.as_ref(), turn_context).await;
    #[cfg(any(test, feature = "test-util"))]
    let upper_trigger = slot.mid_turn_test_upper_trigger().unwrap_or_else(|| {
        token_status
            .auto_compact_scope_limit
            .or(token_status.full_context_window_limit)
            .unwrap_or(i64::MAX)
    });
    #[cfg(not(any(test, feature = "test-util")))]
    let upper_trigger = token_status
        .auto_compact_scope_limit
        .or(token_status.full_context_window_limit)
        .unwrap_or(i64::MAX);

    // Prefer the completed response's usage; do not re-read a later aggregate
    // session snapshot when the seam already carried response-scoped usage.
    let provider_usage = match mid.response_token_usage.as_ref() {
        Some(usage) => token_usage_to_provider_usage_authority(usage),
        None => match sess.token_usage_info().await {
            Some(info) => token_usage_to_provider_usage_authority(&info.last_token_usage),
            None => missing_provider_usage_authority(),
        },
    };
    // Post-measurement: host-captured content after the usage-bearing response
    // through the settled seam (tool results, runtime notes) — not older history.
    let post_measurement_tail = sess
        .estimated_tokens_after_last_model_generated_item()
        .await;
    // N1: include the completed response's own output so next-request growth
    // is not undercounted. Labelled via response-scoped output when present;
    // never double-count the post-measurement tail.
    let response_output_estimate = mid
        .response_token_usage
        .as_ref()
        .map(|u| u.output_tokens.max(0))
        .unwrap_or(0);
    let post_measurement = post_measurement_tail.saturating_add(response_output_estimate);
    let pressure = next_request_pressure(&provider_usage, post_measurement);

    // R3 (CX-S1): the growth-margin hysteresis guard is gone. A prior
    // no-reduction outcome no longer taxes the next attempt — a session under
    // pressure retries at the next seam at zero cost. The attempt record is
    // still kept below, as a diagnostic.

    let host_items = sess
        .clone_history()
        .await
        .raw_items()
        .cloned()
        .collect::<Vec<_>>();
    let mut continuation = work_continuation_for_mid_turn(
        &mid.response_tool_call_ids,
        &host_items,
        mid.total_needs_follow_up,
    );

    // Lower target from LHC continuation profile (or test override).
    #[cfg(any(test, feature = "test-util"))]
    let lower_target = slot
        .mid_turn_test_compact()
        .and_then(|opts| {
            opts.params
                .as_ref()
                .and_then(|p| p.lower_bound)
                .map(|b| b as i64)
        })
        .unwrap_or(DEFAULT_LOWER_TARGET_TOKENS);
    #[cfg(not(any(test, feature = "test-util")))]
    let lower_target = DEFAULT_LOWER_TARGET_TOKENS;

    let provider_identity_valid = !turn_context.config.model_provider_id.is_empty()
        && turn_context
            .config
            .model
            .as_ref()
            .is_some_and(|m| !m.is_empty());

    let fresh_attempt_id = if mid.attempt_id.is_empty() {
        format!(
            "midturn:{}:epoch:{}",
            turn_context.sub_id, mid.input_epoch_at_decision
        )
    } else {
        mid.attempt_id.clone()
    };

    // B1: inspect durable pending boundary / writer claim before a fresh entry.
    // Same-attempt re-entry is the preferred recovery protocol; a stale claim
    // from a dead process is reclaimed below (R5) rather than treated as a live
    // owner. SDK inspection futures are !Send — hop to a dedicated thread (same
    // pattern as the MidTurn worker).
    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);
    // Attempt id a reclaim must re-enter with when durable bookkeeping is
    // unreadable but a writer row still names its (dead) owner.
    let mut reclaim_attempt_id: Option<String> = None;
    let recovery = match inspect_mid_turn_recovery_on_thread(&thread_id, root.clone()).await {
        Ok(r) => r,
        Err(err) => {
            // R4 (CX-S2): a bookkeeping read failure — a corrupt or partially
            // written durable row — is not a reason to strand the turn. The
            // inspect is how the attempt would *prefer* to resume; when it
            // cannot be read the attempt proceeds anyway and the runtime CAS is
            // what prevents a double write. Inspection still never clears
            // durable state.
            warn!(
                %err,
                "LHC MidTurn durable recovery inspect failed; proceeding without stored identity (runtime CAS guards double writes)"
            );
            reclaim_attempt_id = writer_claim_owner_on_thread(&thread_id, root.clone()).await;
            if let Some(owner) = reclaim_attempt_id.as_deref() {
                warn!(
                    owner_attempt = %owner,
                    "LHC MidTurn reclaim receipt: unreadable recovery state still names a writer owner; \
                     re-entering with that attempt id rather than stopping"
                );
            }
            None
        }
    };

    // R5 (CX-S2): a durable writer claim owned by another attempt id is a stale
    // row, not a live competitor. Codex compacts a thread from one process, so
    // the "other" owner is a crashed prior attempt; leaving it authoritative
    // strands the session forever. Re-probe once (the only genuinely racy case
    // is an inspect that observed a claim mid-write), then reclaim.
    let recovery = match recovery {
        Some(rec) if matches!(rec.writer_claim, WriterClaim::Conflict) => {
            let reprobe = inspect_mid_turn_recovery_on_thread(&thread_id, root.clone())
                .await
                .unwrap_or_else(|err| {
                    warn!(%err, "LHC MidTurn writer-claim re-probe failed; reclaiming on the first observation");
                    None
                });
            match reprobe {
                Some(fresh) if !matches!(fresh.writer_claim, WriterClaim::Conflict) => Some(fresh),
                other => {
                    let rec = other.unwrap_or(rec);
                    // Reclaim through the protocol the runtime accepts: re-enter
                    // as the durable owner with the claim reported as ours. A
                    // durable boundary and writer row that name different
                    // attempts is self-inconsistent stale state; the runtime's
                    // own CAS has the last word there (S8/S12, CX-S5). The host
                    // does not add a stop of its own.
                    warn!(
                        owner_attempt = %rec.attempt_id,
                        pending_boundary = rec.pending_boundary,
                        claim_only = rec.claim_only,
                        "LHC MidTurn reclaim receipt: durable writer claim survived re-probe; \
                         prior owner is a dead process (single-writer host), reclaiming and proceeding"
                    );
                    Some(codex_lhc_host::MidTurnRecoveryIdentity {
                        writer_claim: WriterClaim::Lhc,
                        ..rec
                    })
                }
            }
        }
        other => other,
    };

    let (attempt_id, writer_claim, stored_operation_identity) = if let Some(rec) = recovery {
        info!(
            owner_attempt = %rec.attempt_id,
            pending_boundary = rec.pending_boundary,
            claim_only = rec.claim_only,
            "LHC MidTurn re-entering with durable owner attempt for repair/resume"
        );
        // Protocol: boundary repair requires active_non_tool continuation kind.
        // Claim-only: build_host_facts prefers stored continuation/toolCallId.
        if rec.pending_boundary {
            continuation = WorkContinuation::ActiveNonTool;
        } else if let Some(id) = rec.stored_identity.as_ref() {
            // Re-enter with stored continuation kind + toolCallId (preserve-path).
            continuation = match &id.continuation {
                WorkContinuation::PendingCorrelatedToolResult {
                    protected_tool_call_ids,
                    ..
                } => WorkContinuation::PendingCorrelatedToolResult {
                    protected_tool_call_ids: protected_tool_call_ids.clone(),
                    correlation_valid: true,
                },
                other => other.clone(),
            };
        }
        (rec.attempt_id, rec.writer_claim, rec.stored_identity)
    } else if let Some(owner) = reclaim_attempt_id {
        // R4 (CX-S2): unreadable recovery state, live writer row — reclaim it.
        (owner, WriterClaim::Lhc, None)
    } else {
        (fresh_attempt_id, WriterClaim::None, None)
    };

    // LIM-67: capture live-history byte expectations for the protected pair
    // set and required encrypted reasoning BEFORE the attempt mutates anything.
    // The post-install materialized body is validated against these.
    let protected_ids_for_validation: Vec<String> = match &continuation {
        WorkContinuation::PendingCorrelatedToolResult {
            protected_tool_call_ids,
            ..
        } => protected_tool_call_ids.clone(),
        _ => Vec::new(),
    };
    let (protected_pair_expectations, required_encrypted_reasoning) =
        codex_lhc_host::capture_body_expectations(&host_items, &protected_ids_for_validation);

    // LIM-67: pending-tool safe-runway is provider capacity only (full
    // context window). Auto-compact scope is a compaction trigger, not a
    // body-size refuse. Active non-tool seams pass none.
    let pending_tool_seam = matches!(
        continuation,
        WorkContinuation::PendingCorrelatedToolResult { .. }
    );
    let (safe_runway_threshold_tokens, safe_runway_threshold_source) = if pending_tool_seam {
        match token_status.full_context_window_limit {
            Some(limit) => (Some(limit), Some("codex_context_window_limit".to_string())),
            None => (None, None),
        }
    } else {
        (None, None)
    };
    #[cfg(any(test, feature = "test-util"))]
    let (safe_runway_threshold_tokens, safe_runway_threshold_source) =
        match slot.mid_turn_test_safe_runway() {
            Some(t) => (Some(t), Some("test_safe_runway".to_string())),
            None => (safe_runway_threshold_tokens, safe_runway_threshold_source),
        };

    let req = MidTurnCompactContinuationRequest {
        thread_id: thread_id.clone(),
        root: root.clone(),
        attempt_id: attempt_id.clone(),
        provider_usage,
        post_measurement_tokens: post_measurement,
        upper_trigger_tokens: upper_trigger,
        lower_target_tokens: lower_target,
        safe_runway_threshold_tokens,
        safe_runway_threshold_source,
        continuation,
        writer_claim,
        capture_complete: true,
        provider_identity_valid,
        // R1 (CX-S1): the host no longer treats decision-to-apply drift as a
        // stop, so it does not report a delta the engine would re-litigate.
        // Both sides carry the apply-time epoch; the drift itself is logged
        // above.
        input_epoch_at_decision: input_epoch_at_apply,
        input_epoch_at_apply,
        inside_transport_retry: false,
        model_response_complete: mid.model_response_complete,
        compact: {
            let configured =
                compact_opts_with_band_percentages(configured_band_percentages(turn_context));
            #[cfg(any(test, feature = "test-util"))]
            {
                slot.mid_turn_test_compact().or(Some(configured))
            }
            #[cfg(not(any(test, feature = "test-util")))]
            {
                Some(configured)
            }
        },
        stored_operation_identity,
        test_hooks: {
            #[cfg(any(test, feature = "test-util"))]
            {
                slot.mid_turn_test_hooks()
            }
            #[cfg(not(any(test, feature = "test-util")))]
            {
                None
            }
        },
    };

    // SDK futures are !Send — hop to a dedicated thread. Once the LHC operation
    // begins, the hop is an uninterruptible critical section: cancellation may
    // suppress host apply, but never leaves a detached mutator.
    let outcome = match run_mid_turn_on_thread(req, cancellation_token).await {
        Ok(o) => o,
        Err(err) => {
            // R14 (CX-S1): a worker that outruns its bound is detected and
            // warned about, never stranded on. The session proceeds with its
            // current body and compact retries at the next seam.
            let timed_out = matches!(&err, CompactWorkerError::TimedOut(_));
            if timed_out {
                warn!(
                    %err,
                    "LHC MidTurn compact-continuation worker timed out; next provider request continues on the existing body"
                );
            } else {
                error!(%err, "LHC MidTurn compact-continuation operation failed");
            }
            return Ok(LhcCompactAttempt::MidTurnBlocked {
                reason: err.to_string(),
                next_provider_request_allowed: timed_out,
            });
        }
    };

    // R6 (CX-S2): cancellation during the critical section no longer suppresses
    // the host apply. The SDK may have installed a view already; skipping the
    // host rewrite would leave a split state for the next seam to repair. If
    // the turn is really ending the rewrite is harmless; if this is a
    // cancellation race, the session is better off on the smaller body.
    if cancellation_token.is_cancelled() {
        warn!(
            attempt_id = %attempt_id,
            "turn cancelled during MidTurn compact-continuation critical section; \
             applying the installed view anyway (no split state)"
        );
    }

    // R1/R7 (CX-S1): the post-worker input-epoch recheck is gone. Input that
    // arrived during the critical section does not invalidate the view the SDK
    // installed; suppressing the host apply here only left a split state for
    // the next seam to repair. The steer belongs to the next turn.

    if let Some(p) = pressure {
        slot.record_mid_turn_hysteresis(&attempt_id, p, outcome.reduced, &outcome.outcome_kind);
    }

    // LIM-67: a protected-escalation install leaves the durable residual
    // awaiting host validation (next request blocked). The host materializes
    // the exact next provider request from the installed surface, validates
    // it, and records ok/failed before any rollout rewrite or send.
    let awaiting_host_validation = outcome.awaiting_host_validation();
    if outcome.should_rewrite_host_rollout() || awaiting_host_validation {
        // Runtime installed a serving view in LHC; materialize through the
        // existing native-fidelity rewrite path. Host does not synthesize a
        // second continuation marker — CompactMarker here is fork bookkeeping
        // only (covered range + rewrite boundary).
        let host_validation_spec = if awaiting_host_validation {
            Some(codex_lhc_host::BodyValidationSpec {
                attempt_id: attempt_id.clone(),
                protected_tool_call_ids: protected_ids_for_validation.clone(),
                protected_pairs: protected_pair_expectations.clone(),
                required_encrypted_reasoning: required_encrypted_reasoning.clone(),
                safe_runway_threshold_tokens,
            })
        } else {
            None
        };
        let thread_id = handle.thread_id().to_string();
        let root = handle.root().map(std::path::Path::to_path_buf);
        let (_initial_context, world_state_baseline) =
            build_compaction_initial_context(sess.as_ref(), &initial_context_injection).await;
        let reference_context_item = match &initial_context_injection {
            InitialContextInjection::DoNotInject => None,
            InitialContextInjection::BeforeLastUserMessage { .. } => {
                Some(turn_context.to_turn_context_item())
            }
        };

        let marker = match outcome.run.compact_receipt.as_ref() {
            Some(receipt) => CompactMarker::from_receipt(
                receipt,
                &thread_id,
                /*body*/ &[],
                /*archive_tip*/ "midturn",
            ),
            None => {
                return Ok(LhcCompactAttempt::MidTurnBlocked {
                    reason: "LHC install reported but compact receipt missing".into(),
                    next_provider_request_allowed: false,
                });
            }
        };
        // R6 (CX-S2): the SDK has already installed a serving view, so the host
        // apply is no longer cancellable — a suppressed rewrite here is exactly
        // the split state cancellation was supposed to avoid. Install runs
        // under a token that is never cancelled.
        let apply_token = CancellationToken::new();
        let install = install_lhc_compact_rewrite(
            sess,
            turn_context,
            &slot,
            thread_id,
            root,
            marker,
            world_state_baseline,
            reference_context_item,
            /*manual*/ false,
            host_validation_spec,
            &apply_token,
        )
        .await?;
        match install {
            LhcCompactAttempt::Installed { body, marker } => {
                info!(
                    attempt_id = %attempt_id,
                    outcome = %outcome.outcome_kind,
                    marker_persisted = outcome.marker_persisted,
                    "LHC MidTurn compact-continuation installed serving view"
                );
                // AC-7.3/AC-7.4: the SDK opened the continuation turn for this
                // host turn (no host prompt did). Re-bind so the exact identity
                // check at the next seam names the durable turn now active and
                // the thread keeps reaching its typed classification.
                if let (Some(host_turn_id), Some(cont)) = (
                    turn_context
                        .extension_data
                        .get::<codex_lhc_host::LhcTurnId>()
                        .map(|id| id.0.clone()),
                    outcome.continuation_turn_id.as_deref(),
                ) {
                    handle.rebind_turn(&host_turn_id, cont);
                }
                return Ok(LhcCompactAttempt::Installed { body, marker });
            }
            other => {
                error!(?other, "MidTurn host rewrite failed after LHC install");
                return Ok(LhcCompactAttempt::MidTurnBlocked {
                    reason: format!("host rewrite after LHC install failed: {other:?}"),
                    next_provider_request_allowed: false,
                });
            }
        }
    }

    if !outcome.next_provider_request_allowed {
        return Ok(LhcCompactAttempt::MidTurnBlocked {
            reason: format!(
                "compact-continuation outcome={} refuse={:?} skip={:?} reason={}",
                outcome.outcome_kind, outcome.refuse_code, outcome.skip_code, outcome.reason_code
            ),
            next_provider_request_allowed: false,
        });
    }

    // Allowed next request without install (below trigger, skip, no-reduction
    // with valid prior view, etc.).
    Ok(LhcCompactAttempt::MidTurnSkipped {
        reason: format!(
            "compact-continuation outcome={} reason={}",
            outcome.outcome_kind, outcome.reason_code
        ),
    })
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
#[path = "compact_lhc_strict_routing_tests.rs"]
mod strict_routing_tests;

#[cfg(test)]
#[path = "compact_lhc_slice_d_tests.rs"]
mod slice_d_tests;

#[cfg(test)]
#[path = "compact_lhc_mid_turn_tests.rs"]
mod mid_turn_tests;

#[cfg(test)]
#[path = "compact_lhc_canary_tests.rs"]
mod canary_tests;

#[cfg(test)]
#[path = "compact_lhc_readiness_tests.rs"]
mod readiness_tests;
