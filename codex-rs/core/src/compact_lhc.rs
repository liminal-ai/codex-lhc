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

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_analytics::CompactionPhase;
use codex_analytics::CompactionTrigger;
use codex_features::Feature;
use codex_history::RolloutItem;
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

fn configured_band_percentages(turn_context: &TurnContext) -> LhcBandPercentages {
    let percentages = turn_context.config.lhc_compact.percentages;
    LhcBandPercentages {
        full: percentages.full,
        smooth: percentages.smooth,
        detailed: percentages.detailed,
        brief: percentages.brief,
    }
}

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
            let timed_out = is_worker_timeout_reason(&err);
            if timed_out {
                warn!(
                    %err,
                    "LHC MidTurn compact-continuation worker timed out; next provider request continues on the existing body"
                );
            } else {
                error!(%err, "LHC MidTurn compact-continuation operation failed");
            }
            return Ok(LhcCompactAttempt::MidTurnBlocked {
                reason: err,
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
async fn record_host_validation_on_thread(
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
async fn persist_reopen_failure_receipt(
    path: &std::path::Path,
    thread_id: &str,
    root: Option<PathBuf>,
    recorder_frontier_items: u64,
    reopen_error: &str,
) {
    let compacted_rollout = match codex_lhc_host::compacted_rollout_identity(path) {
        Ok(identity) => identity,
        Err(err) => {
            warn!(
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
        warn!(
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
        Ok(()) => info!(
            path = %path.display(),
            recorder_frontier_items,
            "LHC reopen-failure receipt persisted; compacted rollout remains authoritative"
        ),
        Err(err) => warn!(
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
            warn!(%err, "spawn lhc capture-frontier thread failed");
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
async fn writer_claim_owner_on_thread(thread_id: &str, root: Option<PathBuf>) -> Option<String> {
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
            warn!(%err, "spawn lhc-midturn writer-claim probe thread failed");
            return None;
        }
    };
    let owner = rx.await.ok().flatten();
    let _ = tokio::task::spawn_blocking(move || join.join()).await;
    owner
}

/// Inspect durable MidTurn recovery identity on a dedicated thread (SDK
/// inspection futures are `!Send`).
async fn inspect_mid_turn_recovery_on_thread(
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

async fn run_mid_turn_on_thread(
    req: MidTurnCompactContinuationRequest,
    turn_cancel: &CancellationToken,
) -> Result<codex_lhc_host::MidTurnCompactContinuationOutcome, String> {
    // If already cancelled before the critical section, refuse without spawn.
    if turn_cancel.is_cancelled() {
        return Err("lhc-midturn cancelled by turn abort before critical section".into());
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
                    .map_err(|e| format!("runtime: {e}"))?;
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
                        Ok(inner) => inner,
                        Err(_) => Err(format!(
                            "lhc-midturn worker timed out after {}s (operation future dropped on worker)",
                            worker_timeout.as_secs_f64()
                        )),
                    }
                })
            }));
            let out = match result {
                Ok(inner) => inner,
                Err(_) => Err("lhc-midturn thread panicked".into()),
            };
            let _ = tx.send(out);
        })
        .map_err(|e| format!("spawn lhc-midturn thread: {e}"))?;

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
            return Err("lhc-midturn thread panicked during join".into());
        }
        Err(err) => {
            return Err(format!("lhc-midturn join task failed: {err}"));
        }
    }

    // R6 (CX-S2): the worker has finished and may have installed a view. A turn
    // that cancelled while it ran does not un-install that view, so its result
    // is returned and the caller applies it. Suppressing here is what left the
    // split state the next seam had to repair.
    match worker_out {
        Ok(r) => {
            if turn_cancel.is_cancelled() {
                warn!(
                    "lhc-midturn turn cancelled during the critical section; returning the \
                     worker outcome so the installed view is applied"
                );
            }
            r
        }
        Err(_) => Err("lhc-midturn channel closed".into()),
    }
}

/// Why the parts hop produced no outcome. Only `Cancelled` blocks the next
/// provider request (the turn is ending); every `Failed` keeps the current
/// body and retries at a later eligible seam.
#[derive(Debug)]
enum MidTurnPartsHopError {
    Cancelled(String),
    Failed(String),
}

/// Hop `run_mid_turn_parts_compact` onto a bounded current-thread runtime
/// (SDK futures are `!Send`). The SDK install is atomic, so a cancellation
/// during the section never leaves a split state; the worker is always joined.
async fn run_mid_turn_parts_on_thread(
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
    let Some(handle) = slot.get() else {
        return Ok(failed_attempt("capture handle not ready"));
    };

    // Degraded capture is not a hard stop: flush what we can, then rely on
    // archive-coverage validation + host import to retain current content.
    // Flush is bounded — a busy/wedged capture worker must not stall compact
    // before the produce timeout starts.
    if handle.is_degraded() {
        warn!(
            manual,
            "LHC capture is degraded; compact continues (flush + import + coverage check)"
        );
    }
    if !handle.flush_within(COMPACT_FLUSH_BOUND).await {
        warn!(
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
        configured_band_percentages(turn_context),
        cancellation_token,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            if is_cancel_reason(&err) {
                return Ok(cancelled_attempt(err));
            }
            if is_worker_timeout_reason(&err) {
                // R14 (CX-S1): the produce worker outran its bound. Warn and
                // continue on the current usable body; the next eligible seam
                // retries. The ordinary path has no MidTurn result, so this is
                // a non-stranding outcome that lets the turn complete.
                warn!(
                    %err,
                    manual,
                    "LHC compact worker timed out; turn continues on its current body (retry at next seam)"
                );
                return Ok(LhcCompactAttempt::ContinuedWithoutCompact { reason: err });
            }
            warn!(%err, manual, "LHC compact hard failure; preserving history");
            return Ok(failed_attempt(err));
        }
    };

    let produce_body = produced.body.clone();
    if produce_body.is_empty() {
        return Ok(failed_attempt("LHC compact produced empty body"));
    }

    let body_token_estimate = estimate_response_items_tokens(&produce_body);
    if body_token_estimate > baseline_tokens {
        warn!(
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

    info!(
        body_tokens = body_token_estimate,
        auto_compact_limit = ?turn_context
            .config
            .model_auto_compact_token_limit
            .or_else(|| turn_context.model_info.auto_compact_token_limit()),
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

fn is_cancel_reason(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("cancel") || lower.contains("aborted")
}

/// R14 (CX-S1): worker-timeout reasons degrade instead of stranding. The bound
/// still exists — a hung worker is detached at the deadline — but the session
/// keeps its current body and compact retries at the next eligible seam.
fn is_worker_timeout_reason(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("timed out") || lower.contains("timeout")
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
            if is_cancel_reason(&err) {
                return Ok(cancelled_attempt(err));
            }
            warn!(%err, manual, "LHC materialize surfaces unavailable; hard stop");
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
            warn!(
                %err,
                "current_rollout_path failed; installing in memory only (LHC thread view stays the durable source)"
            );
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

    // M2: eligible non-inherited paginated realtime rows must survive this
    // rewrite; they need the ordinal-bearing authority read, not the
    // skip-tolerant item-only one. An unprovable set is a refusal: keep the
    // prior body rather than installing a replacement that silently dropped
    // rows the reader could not prove.
    let prior_realtime_items = match rollout_path.as_ref() {
        Some(path) if path.exists() => match codex_lhc_host::parse_prior_realtime_items(path) {
            Ok(items) => items,
            Err(err) => {
                error!(
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
        error!(%note, manual, "LHC materialize gap_note");
    }

    // M1: a captured item the rebuilt sequence cannot represent exactly is a
    // visible refusal, not a degradation ladder input. Refuse before anything
    // is installed: the session keeps the body it already holds (same
    // disposition as R19) rather than serving a silently altered item.
    if !materialize_result.refusals.is_empty() {
        for refusal in &materialize_result.refusals {
            error!(
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
        warn!(
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
            info!(
                attempt_id = %spec.attempt_id,
                grafted = graft.grafted.len(),
                "LHC MidTurn grafted live protected pairs into materialized body"
            );
        } else {
            warn!(
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
    info!(
        body_tokens = install_tokens,
        auto_compact_limit = ?turn_context
            .config
            .model_auto_compact_token_limit
            .or_else(|| turn_context.model_info.auto_compact_token_limit()),
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
                info!(
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
                warn!(
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
                    warn!(
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
            warn!(
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
        warn!(
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
            error!(
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
                    error!(
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
                        error!(
                            %err,
                            path = %path.display(),
                            "LHC rollout rewrite failed; old file remains authoritative; \
                             preserving in-memory history (no native compact)"
                        );
                        return Ok(failed_attempt(format!("rollout rewrite failed: {err}")));
                    }
                    codex_lhc_host::SwapReconciliation::RestoredOld { detail } => {
                        error!(
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
                        error!(
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
                        warn!(
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
                            warn!(
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
                        error!(
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
                info!(
                    path = %path.display(),
                    items = materialize_result.items.len(),
                    reconciled = reconciled_after_error.is_some(),
                    "LHC rollout rewrite installed (atomic swap)"
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
        error!(
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
        warn!(
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
        warn!(
            %err,
            manual,
            marker_key = %marker.marker_key,
            "LHC compact archive marker note commit failed after write-back; \
             the compacted body stands and the durable record is in the \
             rollout — next open reseeds provenance from it; only this \
             thread's archive lacks the duplicate note"
        );
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

/// Apply the degrade ladder's keep mask to the materialized rollout items so
/// the rewritten file rebuilds exactly the body the session installs (law 1).
///
/// Walks `items` in the order [`history_from_materialized_items`] reads them:
/// the newest `Compacted.replacement_history` (the bands) first, then the
/// post-boundary `ResponseItem` / `InterAgentCommunication` entries (the tail).
/// Positions the mask does not cover are kept — a shorter mask must never
/// silently truncate durable state.
fn drop_materialized_items(items: &mut Vec<RolloutItem>, kept: &[bool]) {
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
    percentages: LhcBandPercentages,
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
            warn!("LHC compact cancelled by turn abort; stopping produce (no native fallback)");
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
                "lhc-compact timed out; detaching worker thread (turn continues on its current body)"
            );
            Err(format!(
                "lhc-compact timed out after {}ms",
                thread_timeout.as_millis()
            ))
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
async fn commit_marker_with_retry(
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
        warn!(
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

#[cfg(test)]
#[path = "compact_lhc_mid_turn_tests.rs"]
mod mid_turn_tests;

#[cfg(test)]
#[path = "compact_lhc_canary_tests.rs"]
mod canary_tests;
