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

/// Build a settled MidTurn seam snapshot.
pub fn settled_mid_turn_seam(
    input_epoch_at_decision: i64,
    input_epoch_at_apply: i64,
    inside_transport_retry: bool,
    capture_flushed: bool,
) -> CompactContinuationSeam {
    CompactContinuationSeam {
        model_response_complete: true,
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
    /// Optional compact profile override (tests use small lower bounds).
    pub compact: Option<HostCompactOpts>,
    /// Test-only fault injection for MidTurn host residual paths (degraded /
    /// invalid install). Production always leaves this `None`; the public
    /// certified entry never accepts hooks.
    pub test_hooks: Option<MidTurnTestHooks>,
}

/// Subset of certified-runtime test hooks exposed for MidTurn host residual
/// coverage. Does not replace production behavior — only injects faults at the
/// same stages the SDK evidence suite already covers.
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
pub fn build_host_facts(req: &MidTurnCompactContinuationRequest) -> CompactContinuationHostFacts {
    CompactContinuationHostFacts {
        attempt_id: req.attempt_id.clone(),
        seam: settled_mid_turn_seam(
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
        policy: CompactContinuationPolicy {
            upper_trigger_tokens: req.upper_trigger_tokens.max(0),
            lower_target_tokens: req.lower_target_tokens.max(0),
            host_capability: CompactContinuationHostCapability::FullStateMachine,
        },
        continuation: req.continuation.clone(),
        writer_claim: req.writer_claim,
        capture_complete: req.capture_complete,
        provider_identity_valid: req.provider_identity_valid,
        single_open_turn: Some(true),
        actor: COMPACT_CONTINUATION_ACTOR.into(),
        harness: HARNESS.into(),
        compact: req.compact.clone(),
    }
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
    // Offline residual coverage may inject fault hooks via test_support only.
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
                ..CompactContinuationTestHooks::default()
            };
            run_compact_continuation_for_tests(ref_, facts, None, Some(mapped)).await
        }
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
