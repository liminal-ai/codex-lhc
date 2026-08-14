//! Codex host adapter for LHC compact-continuation (LIM-63B).
//!
//! Builds validated [`CompactContinuationHostFacts`], runs the certified
//! `run_compact_continuation` operation, and returns residual gates the
//! Codex MidTurn seam must obey. Mutation of the LHC serving view is owned by
//! the runtime; this module never synthesizes a second marker or turn.

use std::path::Path;
use std::path::PathBuf;

use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use lhc::compact_continuation::CompactContinuationHostFacts;
use lhc::compact_continuation::CompactContinuationRunResult;
use lhc::compact_continuation::HostCompactOpts;
use lhc::compact_continuation::run_compact_continuation;
use lhc::shared_tech::compact_continuation::CompactContinuationHostCapability;
use lhc::shared_tech::compact_continuation::CompactContinuationPolicy;
use lhc::shared_tech::compact_continuation::CompactContinuationSeam;
use lhc::shared_tech::compact_continuation::PostMeasurementEstimate;
use lhc::shared_tech::compact_continuation::ProviderUsageAuthority;
use lhc::shared_tech::compact_continuation::ProviderUsageAvailable;
use lhc::shared_tech::compact_continuation::ProviderUsageUnavailable;
use lhc::shared_tech::compact_continuation::ProviderUsageUnavailableReason;
use lhc::shared_tech::compact_continuation::WorkContinuation;
use lhc::shared_tech::compact_continuation::WriterClaim;
use lhc::shared_tech::errors::OpResult;
use lhc::shared_tech::view::PartialViewProfilePercentages;
use lhc::shared_tech::view::ViewCompactParams;
use lhc::threads::ThreadRef;
use serde::Deserialize;
use serde::Serialize;
use tracing::info;
use tracing::warn;

use crate::mapping::HARNESS;
use crate::session::thread_file_path;

/// Actor label for compact-continuation host facts (host, not model).
pub const COMPACT_CONTINUATION_ACTOR: &str = "codex_host";

/// Source label for post-measurement estimates derived from host-captured tail
/// content after the provider usage message.
pub const POST_MEASUREMENT_SOURCE: &str = "lhc_token_estimate";

/// Map OpenAI/Codex [`TokenUsage`] into LHC provider-usage authority without
/// double-counting cached input.
///
/// Provider-reported `input_tokens` is the authoritative total. Components are
/// split so:
/// `non_cached + cache_write + cache_read == input_tokens`.
pub fn token_usage_to_provider_usage_authority(usage: &TokenUsage) -> ProviderUsageAuthority {
    let total = usage.input_tokens.max(0);
    let cache_read = usage.cached_input().clamp(0, total);
    let remaining_after_read = total.saturating_sub(cache_read);
    let cache_write = usage
        .cache_write_input_tokens
        .max(0)
        .clamp(0, remaining_after_read);
    let input = remaining_after_read.saturating_sub(cache_write);
    debug_assert_eq!(input + cache_write + cache_read, total);
    ProviderUsageAuthority::Available(ProviderUsageAvailable {
        available: true,
        input_tokens: input,
        cache_creation_tokens: cache_write,
        cache_read_tokens: cache_read,
        total,
        domain: "provider_reported_input".into(),
    })
}

/// Missing provider usage (authoritative base unavailable).
pub fn missing_provider_usage_authority() -> ProviderUsageAuthority {
    ProviderUsageAuthority::Unavailable(ProviderUsageUnavailable {
        available: false,
        reason: ProviderUsageUnavailableReason::Missing,
        domain: "provider_reported_input".into(),
    })
}

/// Build a MidTurn seam snapshot.
///
/// `model_response_complete` must be true only when the provider stream reached
/// `ResponseEvent::Completed`. Mailbox-preempted / abandoned streams pass false
/// so the certified runtime skips with `not_at_settled_seam`.
pub fn settled_mid_turn_seam(
    input_epoch_at_decision: i64,
    input_epoch_at_apply: i64,
    inside_transport_retry: bool,
    capture_flushed: bool,
) -> CompactContinuationSeam {
    mid_turn_seam(
        /*model_response_complete*/ true,
        input_epoch_at_decision,
        input_epoch_at_apply,
        inside_transport_retry,
        capture_flushed,
    )
}

/// Build a MidTurn seam with an explicit response-complete flag.
pub fn mid_turn_seam(
    model_response_complete: bool,
    input_epoch_at_decision: i64,
    input_epoch_at_apply: i64,
    inside_transport_retry: bool,
    capture_flushed: bool,
) -> CompactContinuationSeam {
    CompactContinuationSeam {
        model_response_complete,
        requested_tools_settled: true,
        capture_flushed,
        before_next_provider_request: true,
        inside_transport_retry,
        input_epoch_at_decision,
        input_epoch_at_apply,
    }
}

/// Continuation branch for the MidTurn compact-continuation seam.
///
/// `response_tool_call_ids` must be the **response-scoped** tool call IDs
/// captured from the just-completed sampling response (not a heuristic rescan
/// of arbitrary older history). History may still be used to validate that
/// settled results exist for those IDs.
///
/// Total continuation intent is separate from the completed response's tool
/// facts:
/// - `pending_correlated_tool_result` when the completed response produced tool
///   calls whose settled results must go on the next provider request;
/// - `active_non_tool` when any other work continues (queued steering/mailbox/
///   hook continuation, end_turn=false, etc.);
/// - `none` only when no next provider request is planned.
///
/// Parallel pairs stay intact. Branch id = lexicographically smallest
/// non-empty response-scoped call_id (deterministic across retries).
pub fn work_continuation_for_mid_turn(
    response_tool_call_ids: &[String],
    history_items: &[ResponseItem],
    total_needs_follow_up: bool,
) -> WorkContinuation {
    if !total_needs_follow_up {
        return WorkContinuation::None;
    }

    let mut response_ids: Vec<String> = response_tool_call_ids
        .iter()
        .filter(|id| !id.is_empty())
        .cloned()
        .collect();
    response_ids.sort();
    response_ids.dedup();

    if response_ids.is_empty() {
        return WorkContinuation::ActiveNonTool;
    }

    let result_ids: std::collections::HashSet<String> = history_items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput { call_id, .. }
            | ResponseItem::CustomToolCallOutput { call_id, .. }
                if !call_id.is_empty() =>
            {
                Some(call_id.clone())
            }
            // ToolSearch is client-correlated when present; scan its output type.
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } if !call_id.is_empty() => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    let mut correlated: Vec<String> = response_ids
        .iter()
        .filter(|id| result_ids.contains(*id))
        .cloned()
        .collect();
    correlated.sort();
    correlated.dedup();

    if let Some(tool_call_id) = correlated.first().cloned() {
        return WorkContinuation::PendingCorrelatedToolResult {
            tool_call_id,
            correlation_valid: true,
        };
    }

    // Response produced tool calls but correlation is incomplete — still a
    // pending-tool shape so the runtime can refuse invalid correlation rather
    // than force a non-tool boundary.
    WorkContinuation::PendingCorrelatedToolResult {
        tool_call_id: response_ids[0].clone(),
        correlation_valid: false,
    }
}

/// History-tail helper retained for offline unit tests of pair-shape scanning.
/// Production MidTurn must use [`work_continuation_for_mid_turn`] with
/// response-scoped IDs.
pub fn work_continuation_from_history_tail(
    items: &[ResponseItem],
    total_needs_follow_up: bool,
) -> WorkContinuation {
    if !total_needs_follow_up {
        return WorkContinuation::None;
    }

    let start = items
        .iter()
        .rposition(is_model_generated_item)
        .map(|i| i.saturating_add(1))
        .unwrap_or(0);

    let model_start = match items.iter().rposition(is_model_generated_item) {
        Some(end) => {
            let mut i = end;
            while i > 0 && is_model_generated_item(&items[i - 1]) {
                i -= 1;
            }
            i
        }
        None => 0,
    };
    let model_window = &items[model_start..start.min(items.len())];
    let response_ids: Vec<String> = model_window
        .iter()
        .filter_map(tool_call_id_of)
        .filter(|s| !s.is_empty())
        .collect();
    work_continuation_for_mid_turn(&response_ids, items, total_needs_follow_up)
}

fn is_model_generated_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role == "assistant",
        ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        _ => false,
    }
}

fn tool_call_id_of(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.clone()),
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => Some(call_id.clone()),
        _ => None,
    }
}

/// Inputs for one MidTurn compact-continuation attempt.
#[derive(Debug, Clone)]
pub struct MidTurnCompactContinuationRequest {
    pub thread_id: String,
    pub root: Option<PathBuf>,
    /// Stable attempt identity from the completed provider response / seam.
    pub attempt_id: String,
    pub provider_usage: ProviderUsageAuthority,
    pub post_measurement_tokens: i64,
    pub upper_trigger_tokens: i64,
    pub lower_target_tokens: i64,
    pub continuation: WorkContinuation,
    pub writer_claim: WriterClaim,
    pub capture_complete: bool,
    pub provider_identity_valid: bool,
    pub input_epoch_at_decision: i64,
    pub input_epoch_at_apply: i64,
    pub inside_transport_retry: bool,
    /// True only when the provider stream settled on `ResponseEvent::Completed`.
    pub model_response_complete: bool,
    /// Optional compact profile override (tests use small lower bounds).
    pub compact: Option<HostCompactOpts>,
    /// When re-entering a durable owner attempt, supply the stored immutable
    /// operation identity so policy/compact/actor/harness/continuation hash
    /// matches. Mutable posture (seam, usage, estimate, capture, writer claim
    /// host assertion) still comes from the live request fields above.
    pub stored_operation_identity: Option<lhc::compact_continuation::StoredOperationIdentity>,
    /// Test-only fault injection for MidTurn host residual paths (degraded /
    /// invalid install). Production always leaves this `None`. The match arm
    /// that routes to SDK `test_support` is compiled only under
    /// `feature = "test-util"`; without that feature non-`None` values are
    /// ignored and the certified public entry is used.
    pub test_hooks: Option<MidTurnTestHooks>,
}

/// Subset of certified-runtime test hooks exposed for MidTurn host residual
/// coverage. Does not replace production behavior — only injects faults at the
/// same stages the SDK evidence suite already covers.
///
/// The type is always nameable so request structs stay stable across feature
/// unification; routing to the SDK fault path requires `feature = "test-util"`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MidTurnTestHooks {
    /// Force `derivations_missing_or_failed` on material facts.
    pub force_derivations_missing_or_failed: Option<bool>,
    /// Force install success/failure after a valid candidate.
    pub force_install_succeeds: Option<bool>,
    /// Fail candidate assembly before install (no marker / no install).
    pub fail_candidate_assembly: bool,
    /// Fail install before the write commits.
    pub fail_install_before_write: bool,
    /// Fail finalize at writer release (claim held, intent present, no release).
    /// Representative claim-only crash window for recovery evidence.
    pub fail_finalize_at_release: bool,
}

/// Host-facing result of one MidTurn attempt.
#[derive(Debug, Clone)]
pub struct MidTurnCompactContinuationOutcome {
    pub run: CompactContinuationRunResult,
    pub next_provider_request_allowed: bool,
    pub installed: bool,
    pub reduced: bool,
    pub outcome_kind: String,
    pub refuse_code: Option<String>,
    pub skip_code: Option<String>,
    pub reason_code: String,
    pub marker_persisted: bool,
    pub continuation_turn_id: Option<String>,
}

impl MidTurnCompactContinuationOutcome {
    pub fn should_rewrite_host_rollout(&self) -> bool {
        self.installed && self.next_provider_request_allowed
    }
}

/// Resolve the on-disk thread path for the capture DB.
pub fn thread_sqlite_path(thread_id: &str, root: Option<&Path>) -> Option<PathBuf> {
    let root = root?;
    Some(thread_file_path(root, thread_id))
}

/// Build validated host facts for the certified runtime.
///
/// When `stored_operation_identity` is present (same-attempt recovery), immutable
/// fields are taken from storage so the intent hash matches; mutable posture
/// (seam, usage, estimate, capture, writer claim, correlationValid) stays live.
///
/// Continuation: use `req.continuation` when the caller already forced
/// `ActiveNonTool` for pending-boundary repair (stored boundary identity is
/// always `active_non_tool`). Otherwise prefer the stored continuation so
/// claim-only preserve-path re-entry keeps the response-scoped toolCallId.
pub fn build_host_facts(req: &MidTurnCompactContinuationRequest) -> CompactContinuationHostFacts {
    let (policy, actor, harness, compact, continuation) =
        if let Some(id) = req.stored_operation_identity.as_ref() {
            let stored_continuation = match &id.continuation {
                WorkContinuation::PendingCorrelatedToolResult { tool_call_id, .. } => {
                    // correlationValid is posture — rebuild true for recovery;
                    // runtime re-proves the pair.
                    WorkContinuation::PendingCorrelatedToolResult {
                        tool_call_id: tool_call_id.clone(),
                        correlation_valid: true,
                    }
                }
                other => other.clone(),
            };
            let continuation = if matches!(req.continuation, WorkContinuation::ActiveNonTool)
                && matches!(id.continuation, WorkContinuation::ActiveNonTool)
            {
                // Boundary repair: request already forced ActiveNonTool and
                // stored identity agrees.
                req.continuation.clone()
            } else if matches!(req.continuation, WorkContinuation::ActiveNonTool)
                && !matches!(id.continuation, WorkContinuation::ActiveNonTool)
            {
                // Unusual: host forced ActiveNonTool while stored identity is
                // different (should not happen for real boundary rows). Prefer
                // stored identity so we never permanent-wedge on hash mismatch.
                stored_continuation
            } else {
                stored_continuation
            };
            (
                id.policy.clone(),
                id.actor.clone(),
                id.harness.clone(),
                id.compact.clone(),
                continuation,
            )
        } else {
            (
                CompactContinuationPolicy {
                    upper_trigger_tokens: req.upper_trigger_tokens.max(0),
                    lower_target_tokens: req.lower_target_tokens.max(0),
                    host_capability: CompactContinuationHostCapability::FullStateMachine,
                },
                COMPACT_CONTINUATION_ACTOR.into(),
                HARNESS.into(),
                req.compact.clone(),
                req.continuation.clone(),
            )
        };

    CompactContinuationHostFacts {
        attempt_id: req.attempt_id.clone(),
        seam: mid_turn_seam(
            req.model_response_complete,
            req.input_epoch_at_decision,
            req.input_epoch_at_apply,
            req.inside_transport_retry,
            req.capture_complete,
        ),
        provider_usage: req.provider_usage.clone(),
        post_measurement_estimate: PostMeasurementEstimate {
            tokens: req.post_measurement_tokens.max(0),
            source: POST_MEASUREMENT_SOURCE.into(),
            domain: "source_labelled_estimate".into(),
        },
        policy,
        continuation,
        writer_claim: req.writer_claim,
        capture_complete: req.capture_complete,
        provider_identity_valid: req.provider_identity_valid,
        single_open_turn: Some(true),
        actor,
        harness,
        compact,
    }
}

/// Inspect durable pending/failed_repairable boundary for host resume/repair.
pub async fn inspect_pending_compact_continuation_boundary(
    thread_id: &str,
    root: Option<&Path>,
) -> Result<Option<lhc::compact_continuation::BoundaryRow>, String> {
    let path = thread_sqlite_path(thread_id, root)
        .ok_or_else(|| "LHC root missing; cannot inspect compact-continuation".to_string())?;
    if !path.exists() {
        return Ok(None);
    }
    let ref_ = ThreadRef::file_path(path.to_string_lossy().into_owned());
    match lhc::compact_continuation::get_pending_compact_continuation_boundary(ref_).await {
        OpResult::Ok { value } => Ok(value),
        OpResult::Err { error } => Err(format!(
            "inspect pending boundary {}: {}",
            error.code.as_str(),
            error.reason
        )),
    }
}

/// Inspect durable writer claim for host resume/repair.
pub async fn inspect_compact_continuation_writer_claim(
    thread_id: &str,
    root: Option<&Path>,
) -> Result<lhc::compact_continuation::WriterClaimRow, String> {
    let path = thread_sqlite_path(thread_id, root)
        .ok_or_else(|| "LHC root missing; cannot inspect compact-continuation".to_string())?;
    if !path.exists() {
        return Err(format!(
            "LHC thread file missing for compact-continuation inspect: {}",
            path.display()
        ));
    }
    let ref_ = ThreadRef::file_path(path.to_string_lossy().into_owned());
    match lhc::compact_continuation::get_compact_continuation_writer_claim(ref_).await {
        OpResult::Ok { value } => Ok(value),
        OpResult::Err { error } => Err(format!(
            "inspect writer claim {}: {}",
            error.code.as_str(),
            error.reason
        )),
    }
}

/// Durable recovery identity for the next MidTurn entry.
///
/// When a pending/failed_repairable boundary or held LHC writer claim exists,
/// the host must re-enter with that exact `attempt_id`. Boundary repair forces
/// `active_non_tool` (protocol). Claim-only recovery re-enters with the
/// **stored** immutable operation identity (continuation, policy, compact,
/// actor/harness) so response-scoped preserve-path toolCallIds still match.
/// Never invents a lease or clears a foreign owner.
#[derive(Debug, Clone, PartialEq)]
pub struct MidTurnRecoveryIdentity {
    pub attempt_id: String,
    pub writer_claim: WriterClaim,
    /// True when a durable pending/failed_repairable boundary owns the attempt.
    pub pending_boundary: bool,
    /// True when only a same-owner writer claim is held (no pending boundary).
    pub claim_only: bool,
    /// Stored immutable operation identity when an attempt-intent row exists.
    /// Required for claim-only exact same-attempt re-entry; optional for
    /// boundary repair (protocol still forces `active_non_tool`).
    pub stored_identity: Option<lhc::compact_continuation::StoredOperationIdentity>,
}

/// Inspect durable attempt-intent / operation identity for recovery.
pub async fn inspect_compact_continuation_attempt_intent(
    thread_id: &str,
    root: Option<&Path>,
    attempt_id: &str,
) -> Result<Option<lhc::compact_continuation::StoredOperationIdentity>, String> {
    let path = thread_sqlite_path(thread_id, root)
        .ok_or_else(|| "LHC root missing; cannot inspect compact-continuation".to_string())?;
    if !path.exists() {
        return Ok(None);
    }
    let ref_ = ThreadRef::file_path(path.to_string_lossy().into_owned());
    match lhc::compact_continuation::get_compact_continuation_attempt_intent(ref_, attempt_id).await
    {
        OpResult::Ok { value } => Ok(value),
        OpResult::Err { error } => Err(format!(
            "inspect attempt intent {}: {}",
            error.code.as_str(),
            error.reason
        )),
    }
}

/// Resolve same-attempt resume identity from durable SDK state.
///
/// Returns `None` when the durable state is clean (caller uses the fresh
/// response-scoped attempt id). Foreign held claims surface as
/// `WriterClaim::Conflict` so the certified runtime refuses without stealing.
/// Inspection failure on a claim-only owner is returned as `Err` so the host
/// does not proceed with a live-seam identity that would permanent-wedge.
pub async fn resolve_mid_turn_recovery_identity(
    thread_id: &str,
    root: Option<&Path>,
) -> Result<Option<MidTurnRecoveryIdentity>, String> {
    let pending = inspect_pending_compact_continuation_boundary(thread_id, root).await?;
    let claim = match inspect_compact_continuation_writer_claim(thread_id, root).await {
        Ok(c) => c,
        Err(_) if pending.is_none() => {
            // Missing thread file with no pending is clean.
            return Ok(None);
        }
        Err(e) => return Err(e),
    };

    if let Some(boundary) = pending {
        // Boundary owner wins; re-enter with that attempt and active_non_tool.
        let writer_claim = if claim.claim == lhc::compact_continuation::WriterClaimKind::Lhc
            && claim.attempt_id.as_deref() == Some(boundary.attempt_id.as_str())
        {
            WriterClaim::Lhc
        } else if claim.claim == lhc::compact_continuation::WriterClaimKind::Lhc
            && claim
                .attempt_id
                .as_ref()
                .is_some_and(|id| id != &boundary.attempt_id)
        {
            // Foreign claim alongside a pending boundary is still not stealable.
            WriterClaim::Conflict
        } else {
            WriterClaim::None
        };
        // Best-effort load of stored identity for actor/policy/compact match.
        // Boundary repair still forces ActiveNonTool continuation; missing
        // identity is non-fatal here (boundary protocol is kind-fixed).
        let stored_identity = match inspect_compact_continuation_attempt_intent(
            thread_id,
            root,
            &boundary.attempt_id,
        )
        .await
        {
            Ok(v) => v,
            Err(err) => {
                // Do not clear state; boundary repair can still proceed with
                // forced active_non_tool if identity inspect fails.
                tracing::warn!(
                    %err,
                    attempt_id = %boundary.attempt_id,
                    "LHC MidTurn boundary recovery: attempt intent inspect failed; continuing with protocol active_non_tool"
                );
                None
            }
        };
        return Ok(Some(MidTurnRecoveryIdentity {
            attempt_id: boundary.attempt_id,
            writer_claim,
            pending_boundary: true,
            claim_only: false,
            stored_identity,
        }));
    }

    if claim.claim == lhc::compact_continuation::WriterClaimKind::Lhc {
        if let Some(owner) = claim.attempt_id {
            // Claim-only: must load stored identity for exact same-attempt re-entry.
            // Without it (or on corruption), surface Err so the host does not
            // re-enter with a live seam that conflicts forever.
            let stored_identity =
                inspect_compact_continuation_attempt_intent(thread_id, root, &owner).await?;
            let Some(stored_identity) = stored_identity else {
                return Err(format!(
                    "claim-only owner attempt {owner} has no durable attempt-intent row; refuse re-entry rather than synthesize identity"
                ));
            };
            return Ok(Some(MidTurnRecoveryIdentity {
                attempt_id: owner,
                writer_claim: WriterClaim::Lhc,
                pending_boundary: false,
                claim_only: true,
                stored_identity: Some(stored_identity),
            }));
        }
    }

    Ok(None)
}

/// Test-oriented compact opts that use a small lower bound so banded compact
/// can run offline without 120k tokens of seed history.
pub fn test_compact_opts(lower_bound: f64) -> HostCompactOpts {
    HostCompactOpts {
        profile: Some("continuation".into()),
        params: Some(ViewCompactParams {
            lower_bound: Some(lower_bound),
            percentages: Some(PartialViewProfilePercentages {
                full: Some(25.0),
                smooth: Some(25.0),
                detailed: Some(25.0),
                brief: Some(25.0),
            }),
        }),
    }
}

/// Test-only: seed a held LHC writer claim without going through claim_lhc_writer.
/// Used for claim-only resume/repair evidence. Feature-gated with test-util.
#[cfg(feature = "test-util")]
pub fn seed_mid_turn_writer_claim_for_tests(
    thread_id: &str,
    root: Option<&Path>,
    attempt_id: &str,
) -> Result<(), String> {
    let path = thread_sqlite_path(thread_id, root).ok_or_else(|| "LHC root missing".to_string())?;
    if !path.exists() {
        return Err(format!("thread file missing: {}", path.display()));
    }
    let path_str = path.to_string_lossy().into_owned();
    let db = match lhc::shared_tech::storage::open_database(&path_str) {
        lhc::shared_tech::errors::OpResult::Ok { value } => value,
        lhc::shared_tech::errors::OpResult::Err { error } => {
            return Err(format!("open db: {}", error.reason));
        }
    };
    lhc::compact_continuation::test_support::seed_writer_claim(
        &db,
        attempt_id,
        "2020-01-01T00:00:00.000Z",
    );
    db.close();
    Ok(())
}

/// Inspect whether a compact-continuation marker event exists for a turn.
pub async fn inspect_has_compact_continuation_marker(
    thread_id: &str,
    root: Option<&Path>,
    continuation_turn_id: &str,
) -> Result<bool, String> {
    let path = thread_sqlite_path(thread_id, root)
        .ok_or_else(|| "LHC root missing; cannot inspect marker".to_string())?;
    if !path.exists() {
        return Ok(false);
    }
    let ref_ = ThreadRef::file_path(path.to_string_lossy().into_owned());
    match lhc::compact_continuation::has_compact_continuation_marker(ref_, continuation_turn_id)
        .await
    {
        OpResult::Ok { value } => Ok(value),
        OpResult::Err { error } => Err(format!(
            "inspect marker {}: {}",
            error.code.as_str(),
            error.reason
        )),
    }
}

/// List compact-continuation receipts for durable positive evidence.
pub async fn inspect_compact_continuation_receipts(
    thread_id: &str,
    root: Option<&Path>,
) -> Result<Vec<lhc::compact_continuation::StoredCompactContinuationReceipt>, String> {
    let path = thread_sqlite_path(thread_id, root)
        .ok_or_else(|| "LHC root missing; cannot inspect receipts".to_string())?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let ref_ = ThreadRef::file_path(path.to_string_lossy().into_owned());
    match lhc::compact_continuation::list_compact_continuation_receipts(ref_, /*limit*/ None).await
    {
        OpResult::Ok { value } => Ok(value),
        OpResult::Err { error } => Err(format!(
            "inspect receipts {}: {}",
            error.code.as_str(),
            error.reason
        )),
    }
}

/// Default production lower target from the LHC continuation profile.
pub const DEFAULT_LOWER_TARGET_TOKENS: i64 = 120_000;

/// Run the certified compact-continuation operation on the capture thread DB.
///
/// Opens a `ThreadRef` against the existing SQLite file (schema v10). Does not
/// open a second capture worker; capture must already be flushed.
pub async fn run_mid_turn_compact_continuation(
    req: MidTurnCompactContinuationRequest,
) -> Result<MidTurnCompactContinuationOutcome, String> {
    let path = thread_sqlite_path(&req.thread_id, req.root.as_deref())
        .ok_or_else(|| "LHC root missing; cannot open compact-continuation thread".to_string())?;
    if !path.exists() {
        return Err(format!(
            "LHC thread file missing for compact-continuation: {}",
            path.display()
        ));
    }

    let facts = build_host_facts(&req);
    let ref_ = ThreadRef::file_path(path.to_string_lossy().into_owned());

    // Production path: public certified entry with no test hooks.
    // Fault injection routes only when `feature = "test-util"` is enabled
    // (pulls `lhc/test-util`). Without that feature, any non-None hooks are
    // ignored so release builds cannot reach the fault path.
    #[cfg(feature = "test-util")]
    let op = match &req.test_hooks {
        None => run_compact_continuation(ref_, facts).await,
        Some(hooks) => {
            use lhc::compact_continuation::test_support::CompactContinuationTestHooks;
            use lhc::compact_continuation::test_support::run_compact_continuation_for_tests;
            let mapped = CompactContinuationTestHooks {
                force_derivations_missing_or_failed: hooks.force_derivations_missing_or_failed,
                force_install_succeeds: hooks.force_install_succeeds,
                fail_candidate_assembly: hooks.fail_candidate_assembly,
                fail_install_before_write: hooks.fail_install_before_write,
                fail_finalize_at_release: hooks.fail_finalize_at_release,
                ..CompactContinuationTestHooks::default()
            };
            run_compact_continuation_for_tests(ref_, facts, None, Some(mapped)).await
        }
    };
    #[cfg(not(feature = "test-util"))]
    let op = {
        debug_assert!(
            req.test_hooks.is_none(),
            "MidTurn test_hooks set without feature = \"test-util\"; ignored"
        );
        run_compact_continuation(ref_, facts).await
    };
    match op {
        OpResult::Ok { value } => {
            let installed = value.compact_receipt.is_some()
                && value
                    .decision
                    .receipt
                    .effects
                    .iter()
                    .any(|e| e.type_str() == "install_serving_view");
            // Truthful reduction comes from the oracle outcome, not a receipt field.
            let outcome_kind = value.decision.outcome.as_str().to_string();
            let reduced = installed
                && outcome_kind != "no_reduction"
                && !matches!(
                    outcome_kind.as_str(),
                    "skip_seam" | "refuse" | "continue_normal" | "normal_complete"
                );
            let outcome = MidTurnCompactContinuationOutcome {
                next_provider_request_allowed: value.next_provider_request_allowed,
                installed,
                reduced,
                outcome_kind,
                refuse_code: value.receipt.refuse_code.map(|c| c.as_str().to_string()),
                skip_code: value.receipt.skip_code.map(|c| c.as_str().to_string()),
                reason_code: value.receipt.reason_code.clone(),
                marker_persisted: value.marker_persisted,
                continuation_turn_id: value.continuation_turn_id.clone(),
                run: value,
            };
            info!(
                attempt_id = %req.attempt_id,
                outcome = %outcome.outcome_kind,
                next_allowed = outcome.next_provider_request_allowed,
                installed = outcome.installed,
                reduced = outcome.reduced,
                marker = outcome.marker_persisted,
                "LHC compact-continuation MidTurn result"
            );
            Ok(outcome)
        }
        OpResult::Err { error } => {
            warn!(
                attempt_id = %req.attempt_id,
                code = %error.code.as_str(),
                reason = %error.reason,
                "LHC compact-continuation operation error"
            );
            Err(format!(
                "compact_continuation {}: {}",
                error.code.as_str(),
                error.reason
            ))
        }
    }
}

/// Default growth margin (tokens) required after a truthful no-reduction before
/// MidTurn may re-attempt compact-continuation. Named / configured for tests.
pub const DEFAULT_HYSTERESIS_GROWTH_MARGIN_TOKENS: i64 = 10_000;

/// Hysteresis after a **truthful** no-reduction (or exact frozen dry-relief
/// outcome). Skip/refusal/capture-lag/transport-retry/input-epoch/invalid-
/// install must **not** suppress later recovery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactContinuationHysteresis {
    pub last_attempt_id: String,
    pub last_pressure_tokens: i64,
    pub last_reduced: bool,
    pub last_outcome: String,
    /// Required measured growth above `last_pressure_tokens` to clear.
    pub growth_margin_tokens: i64,
    /// True only while a truthful no-reduction treadmill guard is armed.
    pub armed: bool,
}

impl Default for CompactContinuationHysteresis {
    fn default() -> Self {
        Self {
            last_attempt_id: String::new(),
            last_pressure_tokens: 0,
            last_reduced: true,
            last_outcome: String::new(),
            growth_margin_tokens: DEFAULT_HYSTERESIS_GROWTH_MARGIN_TOKENS,
            armed: false,
        }
    }
}

impl CompactContinuationHysteresis {
    pub fn with_growth_margin(mut self, margin: i64) -> Self {
        self.growth_margin_tokens = margin.max(0);
        self
    }

    /// Whether another attempt is warranted given new measured pressure.
    /// When armed after truthful no-reduction, requires pressure growth of at
    /// least `growth_margin_tokens` (default 10k).
    pub fn should_attempt_after_no_reduction(&self, next_pressure_tokens: i64) -> bool {
        if !self.armed {
            return true;
        }
        next_pressure_tokens
            >= self
                .last_pressure_tokens
                .saturating_add(self.growth_margin_tokens)
    }

    /// Record an attempt. Only truthful no-reduction (or frozen dry-relief
    /// alias) arms the treadmill guard. Successful reduction clears it.
    /// Skip/refuse/capture/transport outcomes leave prior state alone.
    pub fn record(&mut self, attempt_id: &str, pressure: i64, reduced: bool, outcome: &str) {
        if reduced {
            self.clear(attempt_id, pressure, outcome);
            return;
        }
        if is_truthful_no_reduction_outcome(outcome) {
            self.last_attempt_id = attempt_id.to_string();
            self.last_pressure_tokens = pressure;
            self.last_reduced = false;
            self.last_outcome = outcome.to_string();
            self.armed = true;
        }
        // Non-suppressive outcomes: leave armed state unchanged so recovery
        // after capture lag / transport retry / epoch change still works.
    }

    pub fn clear(&mut self, attempt_id: &str, pressure: i64, outcome: &str) {
        self.last_attempt_id = attempt_id.to_string();
        self.last_pressure_tokens = pressure;
        self.last_reduced = true;
        self.last_outcome = outcome.to_string();
        self.armed = false;
    }
}

fn is_truthful_no_reduction_outcome(outcome: &str) -> bool {
    matches!(
        outcome,
        "no_reduction" | "terminal_no_reduction" | "dry_relief_no_reduction"
    )
}

/// Next-request pressure = provider total + post-measurement estimate.
pub fn next_request_pressure(usage: &ProviderUsageAuthority, estimate_tokens: i64) -> Option<i64> {
    usage
        .available_total()
        .map(|t| t.saturating_add(estimate_tokens.max(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn usage_mapping_splits_without_double_counting_cache() {
        let usage = TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_write_input_tokens: 10,
            output_tokens: 5,
            reasoning_output_tokens: 0,
            total_tokens: 105,
            codex_rollout_budget_units: None,
        };
        let auth = token_usage_to_provider_usage_authority(&usage);
        match auth {
            ProviderUsageAuthority::Available(a) => {
                assert_eq!(a.input_tokens, 50);
                assert_eq!(a.cache_creation_tokens, 10);
                assert_eq!(a.cache_read_tokens, 40);
                assert_eq!(a.total, 100);
                assert_eq!(
                    a.input_tokens + a.cache_creation_tokens + a.cache_read_tokens,
                    a.total
                );
            }
            ProviderUsageAuthority::Unavailable(_) => panic!("expected available"),
        }
    }

    #[test]
    fn usage_mapping_clamps_when_cache_exceeds_total() {
        let usage = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 20,
            cache_write_input_tokens: 5,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 10,
            codex_rollout_budget_units: None,
        };
        let auth = token_usage_to_provider_usage_authority(&usage);
        match auth {
            ProviderUsageAuthority::Available(a) => {
                assert_eq!(a.total, 10);
                assert_eq!(
                    a.input_tokens + a.cache_creation_tokens + a.cache_read_tokens,
                    a.total
                );
            }
            ProviderUsageAuthority::Unavailable(_) => panic!("expected available"),
        }
    }

    #[test]
    fn parallel_tool_branch_picks_lexicographically_smallest_correlated_id() {
        let items = vec![
            ResponseItem::Message {
                id: None,
                role: "assistant".into(),
                content: vec![],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "a".into(),
                namespace: None,
                arguments: "{}".into(),
                encrypted_function_args: None,
                call_id: "call-b".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "b".into(),
                namespace: None,
                arguments: "{}".into(),
                encrypted_function_args: None,
                call_id: "call-a".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-b".into(),
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text("b".into()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-a".into(),
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text("a".into()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        match work_continuation_from_history_tail(&items, true) {
            WorkContinuation::PendingCorrelatedToolResult {
                tool_call_id,
                correlation_valid,
            } => {
                assert_eq!(tool_call_id, "call-a");
                assert!(correlation_valid);
            }
            other => panic!("expected pending tool, got {other:?}"),
        }
    }

    #[test]
    fn active_non_tool_when_follow_up_without_tools() {
        let items = vec![ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        assert_eq!(
            work_continuation_from_history_tail(&items, true),
            WorkContinuation::ActiveNonTool
        );
        assert_eq!(
            work_continuation_from_history_tail(&items, false),
            WorkContinuation::None
        );
    }

    #[test]
    fn web_search_with_queued_steering_is_active_non_tool() {
        // Server-side WebSearch must not enter response_tool_call_ids; with
        // queued steering the branch is active_non_tool (not invalid pending-tool).
        let items = vec![
            ResponseItem::WebSearchCall {
                id: Some(codex_protocol::ResponseItemId::from_server("ws-1".into())),
                status: Some("completed".into()),
                action: Some(codex_protocol::models::WebSearchAction::Search {
                    query: Some("q".into()),
                    queries: None,
                }),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".into(),
                content: vec![],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        // Empty response-scoped client-executed ids + follow-up → ActiveNonTool.
        assert_eq!(
            work_continuation_for_mid_turn(&[], &items, true),
            WorkContinuation::ActiveNonTool
        );
    }

    #[test]
    fn tool_search_call_and_output_is_valid_pending_tool() {
        let items = vec![
            ResponseItem::ToolSearchCall {
                id: None,
                call_id: Some("ts-1".into()),
                status: Some("completed".into()),
                execution: "client".into(),
                arguments: serde_json::json!({}),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ToolSearchOutput {
                id: None,
                call_id: Some("ts-1".into()),
                status: "completed".into(),
                execution: "client".into(),
                tools: vec![],
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        match work_continuation_for_mid_turn(&["ts-1".into()], &items, true) {
            WorkContinuation::PendingCorrelatedToolResult {
                tool_call_id,
                correlation_valid,
            } => {
                assert_eq!(tool_call_id, "ts-1");
                assert!(correlation_valid);
            }
            other => panic!("expected valid pending tool, got {other:?}"),
        }
    }

    #[test]
    fn hysteresis_blocks_until_growth_margin_after_no_reduction() {
        let mut h = CompactContinuationHysteresis::default();
        h.record("a1", 100_000, false, "no_reduction");
        assert!(h.armed);
        assert!(!h.should_attempt_after_no_reduction(100_000));
        assert!(!h.should_attempt_after_no_reduction(109_999));
        assert!(h.should_attempt_after_no_reduction(110_000));
    }

    #[test]
    fn hysteresis_table_truthful_only() {
        let cases = [
            ("no_reduction", false, true, false),
            ("terminal_no_reduction", false, true, false),
            ("dry_relief_no_reduction", false, true, false),
            ("skip_seam", false, false, true),
            ("refuse", false, false, true),
            ("continue_normal", false, false, true),
            ("compact_continue_turn", true, false, true),
            ("degraded_compact", true, false, true),
        ];
        for (outcome, reduced, expect_armed, expect_attempt_same) in cases {
            let mut h = CompactContinuationHysteresis::default();
            h.record("t", 50_000, reduced, outcome);
            assert_eq!(
                h.armed, expect_armed,
                "outcome={outcome} reduced={reduced} armed"
            );
            assert_eq!(
                h.should_attempt_after_no_reduction(50_000),
                expect_attempt_same,
                "outcome={outcome} same-pressure attempt"
            );
        }
    }

    #[test]
    fn hysteresis_clears_on_successful_reduction() {
        let mut h = CompactContinuationHysteresis::default();
        h.record("a1", 100_000, false, "no_reduction");
        assert!(!h.should_attempt_after_no_reduction(100_000));
        h.record("a2", 100_000, true, "compact_continue_turn");
        assert!(!h.armed);
        assert!(h.should_attempt_after_no_reduction(100_000));
    }

    #[test]
    fn hysteresis_skip_does_not_suppress_recovery() {
        let mut h = CompactContinuationHysteresis::default();
        // Prior skip must not arm.
        h.record("skip1", 90_000, false, "skip_seam");
        assert!(!h.armed);
        assert!(h.should_attempt_after_no_reduction(90_000));
        // Capture lag / transport / epoch refuse aliases.
        for outcome in [
            "skip_capture_incomplete",
            "input_epoch_changed",
            "inside_transport_retry",
            "invalid_install",
        ] {
            let mut h = CompactContinuationHysteresis::default();
            h.record("x", 80_000, false, outcome);
            assert!(!h.armed, "{outcome} must not arm hysteresis");
            assert!(h.should_attempt_after_no_reduction(80_000));
        }
    }

    #[test]
    fn pending_tool_uses_response_scoped_ids_not_older_history() {
        let history = vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "old".into(),
                namespace: None,
                arguments: "{}".into(),
                encrypted_function_args: None,
                call_id: "old-call".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "old-call".into(),
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text("old".into()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "new".into(),
                namespace: None,
                arguments: "{}".into(),
                encrypted_function_args: None,
                call_id: "new-call".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "new-call".into(),
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text("new".into()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        match work_continuation_for_mid_turn(
            &["new-call".into()],
            &history,
            /*total_needs_follow_up*/ true,
        ) {
            WorkContinuation::PendingCorrelatedToolResult {
                tool_call_id,
                correlation_valid,
            } => {
                assert_eq!(tool_call_id, "new-call");
                assert!(correlation_valid);
            }
            other => panic!("expected new-call branch, got {other:?}"),
        }
    }

    #[test]
    fn queued_input_only_is_active_non_tool() {
        assert_eq!(
            work_continuation_for_mid_turn(&[], &[], /*total_needs_follow_up*/ true,),
            WorkContinuation::ActiveNonTool
        );
        assert_eq!(
            work_continuation_for_mid_turn(&[], &[], /*total_needs_follow_up*/ false,),
            WorkContinuation::None
        );
    }
}
