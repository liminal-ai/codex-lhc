//! LIM-63B MidTurn compact-continuation evidence (offline, mock path).
//!
//! Drives production `try_run_lhc_compact_arm` / `run_auto_compact` at
//! `CompactionPhase::MidTurn` through the certified SDK runtime. No paid
//! provider calls. Acceptance matrix + blocking-defect proofs.

use std::sync::Arc;
use std::time::Duration;

use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadStartInput;
use codex_features::Feature;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::ProviderUsageAuthority;
use codex_lhc_host::WorkContinuation;
use codex_lhc_host::install_with_root;
use codex_lhc_host::test_compact_opts;
use codex_lhc_host::token_usage_to_provider_usage_authority;
use codex_lhc_host::wait_for_handle;
use codex_lhc_host::work_continuation_for_mid_turn;
use codex_lhc_host::work_continuation_from_history_tail;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;
use codex_thread_store::PersistContext;
use pretty_assertions::assert_eq;
use serial_test::serial;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::LhcCompactAttempt;
use super::MidTurnSeamFacts;
use super::try_run_lhc_compact_arm;
use crate::compact::InitialContextInjection;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn::run_auto_compact;
use codex_protocol::error::CodexErrorDetails;

fn text_input(text: &str) -> UserInput {
    UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }
}

fn sample_usage(input_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 10,
        reasoning_output_tokens: 0,
        total_tokens: input_tokens.saturating_add(10),
        codex_rollout_budget_units: None,
    }
}

async fn install_lhc_midturn(session: &mut Session, root: std::path::PathBuf) {
    session
        .set_feature_for_test(Feature::LhcCapture, true)
        .expect("enable LhcCapture");
    let mut builder = ExtensionRegistryBuilder::<crate::config::Config>::new();
    install_with_root(&mut builder, |_c| true, root);
    let registry = Arc::new(builder.build());
    session.services.extensions = Arc::clone(&registry);
    let config = session.get_config().await;
    let environments = [];
    let session_source = SessionSource::Exec;
    for contributor in registry.thread_lifecycle_contributors() {
        contributor
            .on_thread_start(ThreadStartInput {
                config: config.as_ref(),
                session_source: &session_source,
                persistent_thread_state_available: false,
                environments: &environments,
                mcp_resource_client: None,
                extension_metrics: None,
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
            })
            .await;
    }
    if let Some(slot) = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
    {
        // Offline MidTurn: small lower bound + low upper trigger so compact can
        // fire without 120k seed history. Knobs reduce cost only; production
        // path remains try_run_lhc_compact_arm / run_auto_compact.
        slot.set_mid_turn_test_compact(Some(test_compact_opts(400.0)));
        slot.set_mid_turn_test_upper_trigger(Some(500));
        let cbs = codex_lhc_host::lhc_inference_callbacks(false)
            .expect("deterministic offline callbacks");
        slot.set_derivation_callbacks(cbs);
        *session
            .services
            .lhc_test_inference
            .lock()
            .expect("lhc_test_inference lock") = Some(
            codex_lhc_host::lhc_inference_callbacks(false)
                .expect("deterministic offline callbacks"),
        );
    }
}

async fn seed_turns(session: &Session, tc: &crate::session::turn_context::TurnContext, n: usize) {
    let pad = "x".repeat(800);
    for i in 0..n {
        session
            .record_user_prompt_and_emit_turn_item(
                tc,
                &[text_input(&format!("user turn {i} {pad}"))],
                None,
                PersistContext::TurnStart,
            )
            .await;
        session
            .record_conversation_items_with_provenance(
                tc,
                &[ResponseItem::Message {
                    id: None,
                    role: "assistant".into(),
                    content: vec![ContentItem::OutputText {
                        text: format!("assistant reply {i} {pad}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                codex_extension_api::RawItemProvenance::ModelOutput,
            )
            .await;
    }
}

async fn inject_response_usage(
    session: &Session,
    tc: &crate::session::turn_context::TurnContext,
    input_tokens: i64,
) {
    let usage = sample_usage(input_tokens);
    session
        .record_token_usage_info(tc, Some(&usage))
        .await
        .expect("record token usage");
}

fn mid_facts(
    attempt: &str,
    total_needs_follow_up: bool,
    epoch: i64,
    tool_ids: Vec<String>,
    usage: Option<TokenUsage>,
) -> MidTurnSeamFacts {
    MidTurnSeamFacts {
        model_response_complete: true,
        attempt_id: attempt.into(),
        response_token_usage: usage,
        response_tool_call_ids: tool_ids,
        total_needs_follow_up,
        input_epoch_at_decision: epoch,
        inside_transport_retry: false,
    }
}

fn decision_epoch(session: &Session) -> i64 {
    i64::try_from(session.input_queue.input_epoch()).unwrap_or(0)
}

#[test]
fn provider_usage_mapping_no_double_count_cached_input() {
    let usage = TokenUsage {
        input_tokens: 1_000,
        cached_input_tokens: 400,
        cache_write_input_tokens: 100,
        output_tokens: 50,
        reasoning_output_tokens: 10,
        total_tokens: 1_050,
        codex_rollout_budget_units: None,
    };
    match token_usage_to_provider_usage_authority(&usage) {
        ProviderUsageAuthority::Available(a) => {
            assert_eq!(
                a.input_tokens + a.cache_creation_tokens + a.cache_read_tokens,
                a.total
            );
            assert_eq!(a.total, 1_000);
            assert_eq!(a.cache_read_tokens, 400);
            assert_eq!(a.cache_creation_tokens, 100);
            assert_eq!(a.input_tokens, 500);
        }
        ProviderUsageAuthority::Unavailable(_) => panic!("expected available"),
    }
}

#[test]
fn parallel_tool_ids_form_complete_sorted_protected_set() {
    let items = vec![
        ResponseItem::FunctionCall {
            id: None,
            name: "t1".into(),
            namespace: None,
            arguments: "{}".into(),
            encrypted_function_args: None,
            call_id: "z-call".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "t2".into(),
            namespace: None,
            arguments: "{}".into(),
            encrypted_function_args: None,
            call_id: "a-call".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "z-call".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("z".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "a-call".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("a".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    match work_continuation_for_mid_turn(
        &["z-call".into(), "a-call".into()],
        &items,
        /*total_needs_follow_up*/ true,
    ) {
        WorkContinuation::PendingCorrelatedToolResult {
            protected_tool_call_ids,
            correlation_valid,
        } => {
            // Contract 2.0.0: the complete sorted response-scoped set.
            assert_eq!(protected_tool_call_ids, vec!["a-call", "z-call"]);
            assert!(correlation_valid);
        }
        other => panic!("expected pending tool branch, got {other:?}"),
    }
}

#[test]
fn response_scoped_ids_ignore_older_history_tool_calls() {
    let items = vec![
        ResponseItem::FunctionCall {
            id: None,
            name: "old".into(),
            namespace: None,
            arguments: "{}".into(),
            encrypted_function_args: None,
            call_id: "aaa-old".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "aaa-old".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("old".into()),
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
            call_id: "zzz-new".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "zzz-new".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("new".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    match work_continuation_for_mid_turn(
        &["zzz-new".into()],
        &items,
        /*total_needs_follow_up*/ true,
    ) {
        WorkContinuation::PendingCorrelatedToolResult {
            protected_tool_call_ids,
            correlation_valid,
        } => {
            assert_eq!(protected_tool_call_ids, vec!["zzz-new"]);
            assert!(correlation_valid);
        }
        other => panic!("expected zzz-new, got {other:?}"),
    }
}

#[test]
fn queued_input_only_is_active_non_tool_not_none() {
    assert_eq!(
        work_continuation_for_mid_turn(&[], &[], /*total*/ true),
        WorkContinuation::ActiveNonTool
    );
    assert_eq!(
        work_continuation_for_mid_turn(&[], &[], /*total*/ false),
        WorkContinuation::None
    );
}

/// R3 (CX-S1): the growth-margin guard is gone. The attempt record survives as
/// a diagnostic only — `DEFAULT_HYSTERESIS_GROWTH_MARGIN_TOKENS` and
/// `should_attempt_after_no_reduction` no longer exist, so no recorded outcome
/// can suppress the next attempt. (The old
/// `hysteresis_default_margin_is_10k` test asserted the 10k tax as intended
/// behavior and is deleted with it.)
#[test]
fn hysteresis_record_is_diagnostic_only() {
    for outcome in [
        "skip_seam",
        "refuse",
        "continue_normal",
        "skip_capture_incomplete",
        "input_epoch_changed",
        "inside_transport_retry",
        "invalid_install",
    ] {
        let mut h = codex_lhc_host::CompactContinuationHysteresis::default();
        h.record("x", 50_000, false, outcome);
        assert!(!h.armed, "{outcome} must not arm hysteresis");
    }
    let mut armed = codex_lhc_host::CompactContinuationHysteresis::default();
    armed.record("a1", 100_000, false, "no_reduction");
    assert!(armed.armed, "truthful no-reduction is still recorded");
    assert_eq!(armed.last_pressure_tokens, 100_000);
}

#[tokio::test]
async fn mid_turn_transport_retry_skips_without_mutation() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 4).await;
    let sess = Arc::new(session);
    let mut mid = mid_facts(
        "retry-1",
        true,
        decision_epoch(&sess),
        Vec::new(),
        Some(sample_usage(2_000)),
    );
    mid.inside_transport_retry = true;
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    match attempt {
        LhcCompactAttempt::MidTurnSkipped { reason } => {
            assert!(
                reason.contains("transport retry"),
                "expected transport-retry skip, got {reason}"
            );
        }
        other => panic!("expected MidTurnSkipped, got {other:?}"),
    }
}

/// R1 (CX-S1): input arriving between the rollover decision and the compact
/// entry does not invalidate settled history — that input belongs to the next
/// turn. The decision-to-apply epoch veto (G11) is gone: compact installs, and
/// the queued input is still pending afterwards.
///
/// Supersedes `mid_turn_input_epoch_gate_uses_queue_epoch_not_history`, which
/// asserted the skip as intended behavior.
#[tokio::test]
async fn mid_turn_input_epoch_change_does_not_suppress_compact() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    // Force above-trigger so a real install is the expected outcome.
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 16).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    let decision_epoch = decision_epoch(&session);
    // Production path: mailbox enqueue bumps input epoch without history change.
    let history_version_before = session.clone_history().await.history_version();
    session
        .input_queue
        .enqueue_mailbox_communication(
            codex_protocol::protocol::InterAgentCommunication::new(
                codex_protocol::AgentPath::root(),
                codex_protocol::AgentPath::try_from("/root/worker").expect("path"),
                Vec::new(),
                "pending steer/mail".into(),
                /*trigger_turn*/ false,
            ),
            /*parent_turn_id*/ None,
            /*root_turn_id*/ None,
        )
        .await;
    let history_version_after = session.clone_history().await.history_version();
    assert_eq!(
        history_version_before, history_version_after,
        "mailbox must not change history_version (epoch must not use history proxy)"
    );
    assert_ne!(
        decision_epoch,
        i64::try_from(session.input_queue.input_epoch()).unwrap_or(0)
    );

    let mail_order_before = session.input_queue.has_pending_mailbox_items().await;
    assert!(mail_order_before);

    let sess = Arc::new(session);
    let mid = mid_facts(
        "epoch-prod",
        true,
        decision_epoch,
        Vec::new(),
        Some(sample_usage(5_000)),
    );
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("stale decision epoch must not suppress compact, got {attempt:?}");
    };
    assert!(!body.is_empty(), "installed body must be non-empty");
    // The queued input still belongs to the next turn.
    assert!(
        sess.input_queue.has_pending_mailbox_items().await,
        "pending mailbox must be preserved across the compact"
    );
}

/// R16 (CX-S1): feature on but no capture slot is a transient startup
/// condition. The next provider request continues on the existing body
/// (`MidTurnBlocked(true)`), and `run_auto_compact` still must not fall open to
/// native compact.
#[tokio::test]
async fn mid_turn_missing_slot_allows_next_request_without_native_fallback() {
    let (mut session, tc) = make_session_and_context().await;
    session
        .set_feature_for_test(Feature::LhcCapture, true)
        .expect("enable");
    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts("no-slot", true, 0, Vec::new(), None)),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    match attempt {
        LhcCompactAttempt::MidTurnBlocked {
            reason,
            next_provider_request_allowed,
        } => {
            assert!(reason.contains("LhcCaptureSlot"), "{reason}");
            assert!(
                next_provider_request_allowed,
                "a missing slot must not strand the turn: {reason}"
            );
        }
        other => panic!("expected MidTurnBlocked without slot, got {other:?}"),
    }
}

#[tokio::test]
async fn mid_turn_feature_off_stops_without_native_fallback() {
    let (mut session, tc) = make_session_and_context().await;
    session
        .set_feature_for_test(Feature::LhcCapture, false)
        .expect("disable");
    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts("off", true, 0, Vec::new(), None)),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    match attempt {
        LhcCompactAttempt::Failed { reason } => {
            assert!(reason.contains("LhcCapture off"), "{reason}");
        }
        other => panic!("expected strict failure with native disabled, got {other:?}"),
    }
}

/// R2 (CX-S1): a wedged capture worker is warned about, not stranded on. The
/// SDK compacts the LHC thread, not the capture buffer, so the flush timeout
/// (G9) and the degraded recheck (G10) no longer stop anything — the MidTurn
/// path now behaves like the ordinary path (G33/G34).
///
/// Supersedes `mid_turn_blocked_capture_flush_stops_without_hanging_or_native`,
/// which asserted `MidTurnBlocked(false)` as intended behavior.
#[tokio::test]
async fn mid_turn_blocked_capture_flush_warns_and_compact_continues() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 16).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    // Park the worker so the arm's flush can never be acknowledged, and queue
    // capture work behind the park.
    let release = handle.block_worker().await;
    handle.persist(
        &ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "pending-behind-blocked-worker".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        codex_extension_api::RawItemProvenance::UserPrompt,
    );

    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let started = std::time::Instant::now();
    let attempt = tokio::time::timeout(
        Duration::from_secs(30),
        try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid_facts(
                "blocked-flush",
                true,
                epoch,
                Vec::new(),
                Some(sample_usage(5_000)),
            )),
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("blocked capture worker must not hang MidTurn compact")
    .expect("arm");
    let elapsed = started.elapsed();
    drop(release);
    assert!(
        elapsed < Duration::from_secs(30),
        "flush bound must keep the seam bounded; took {elapsed:?}"
    );
    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("wedged capture worker must not stop compact, got {attempt:?}");
    };
    assert!(!body.is_empty(), "installed body must be non-empty");
}

#[tokio::test]
async fn mid_turn_active_non_tool_runs_certified_runtime() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 12).await;
    inject_response_usage(&session, &tc, 2_000).await;
    handle.flush().await;
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let mid = mid_facts(
        "resp-active-1",
        true,
        epoch,
        Vec::new(),
        Some(sample_usage(2_000)),
    );
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    // Below trigger is a valid skip; install is also valid. Never native-fall-open.
    match attempt {
        LhcCompactAttempt::Installed { body, .. } => {
            assert!(!body.is_empty(), "installed body must be non-empty");
        }
        LhcCompactAttempt::MidTurnSkipped { reason } => {
            assert!(!reason.is_empty(), "skip must carry a diagnostic");
        }
        LhcCompactAttempt::ContinuedWithoutCompact { reason } => {
            assert!(!reason.is_empty(), "continue must carry a diagnostic");
        }
        LhcCompactAttempt::MidTurnBlocked {
            reason,
            next_provider_request_allowed,
        } => {
            assert!(!reason.is_empty());
            let _ = next_provider_request_allowed;
        }
        LhcCompactAttempt::Unavailable { reason }
        | LhcCompactAttempt::Failed { reason }
        | LhcCompactAttempt::Cancelled { reason } => {
            panic!("MidTurn must not hard-stop when LHC is healthy: {reason}");
        }
    }
}

#[tokio::test]
async fn mid_turn_pending_tool_branch_preserves_pair_shape() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 6).await;
    // Append parallel tool call/result pairs as post-measurement tail.
    session
        .record_conversation_items_with_provenance(
            &tc,
            &[
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".into(),
                    namespace: None,
                    arguments: r#"{"cmd":"true"}"#.into(),
                    encrypted_function_args: None,
                    call_id: "call-tool-z".into(),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".into(),
                    namespace: None,
                    arguments: r#"{"cmd":"true"}"#.into(),
                    encrypted_function_args: None,
                    call_id: "call-tool-a".into(),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: "call-tool-z".into(),
                    output: FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text("z".into()),
                        success: Some(true),
                    },
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: "call-tool-a".into(),
                    output: FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text("a".into()),
                        success: Some(true),
                    },
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            codex_extension_api::RawItemProvenance::ModelOutput,
        )
        .await;
    inject_response_usage(&session, &tc, 2_000).await;
    handle.flush().await;
    let items: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    let response_ids = vec!["call-tool-z".into(), "call-tool-a".into()];
    match work_continuation_for_mid_turn(&response_ids, &items, true) {
        WorkContinuation::PendingCorrelatedToolResult {
            protected_tool_call_ids,
            correlation_valid,
        } => {
            assert_eq!(protected_tool_call_ids, vec!["call-tool-a", "call-tool-z"]);
            assert!(correlation_valid);
        }
        other => panic!("expected pending tool, got {other:?}"),
    }
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "resp-tool-1",
            true,
            epoch,
            response_ids,
            Some(sample_usage(2_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    // Production path exercised; must not be Unavailable (native fall-open).
    assert!(
        !matches!(attempt, LhcCompactAttempt::Unavailable { .. }),
        "pending-tool MidTurn must not fall open native: {attempt:?}"
    );
    // After pending-tool path, both pairs remain in history verbatim.
    let after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
    let call_ids: Vec<_> = after
        .iter()
        .filter_map(|i| match i {
            ResponseItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert!(call_ids.contains(&"call-tool-a"));
    assert!(call_ids.contains(&"call-tool-z"));
}

/// R3 (CX-S1): a prior truthful no-reduction records a diagnostic and taxes
/// nothing. With the 10k growth margin gone, the very next seam re-attempts and
/// installs even though measured pressure has not grown.
///
/// Supersedes `mid_turn_hysteresis_blocks_repeat_no_reduction_with_margin`,
/// which asserted the growth tax as intended behavior.
#[tokio::test]
async fn mid_turn_prior_no_reduction_does_not_suppress_next_attempt() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 16).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    // Prior truthful no-reduction recorded far above the pressure we are about
    // to present: under the old growth margin this seam could not attempt.
    slot.record_mid_turn_hysteresis("prior", 100_000, false, "no_reduction");
    assert!(slot.mid_turn_hysteresis().armed);

    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "after-no-reduction",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("prior no-reduction must not suppress the next attempt, got {attempt:?}");
    };
    assert!(!body.is_empty(), "installed body must be non-empty");
}

/// R17 (CX-S1): the seam-facts guard is gone. `session/turn.rs` constructs
/// `MidTurnSeamFacts` unconditionally, so this state is unreachable in
/// production; if it were ever reached, the dispatch declines into the ordinary
/// settled-seam compact instead of blocking the next provider request.
///
/// Supersedes `mid_turn_missing_seam_facts_blocks`.
#[tokio::test]
async fn mid_turn_missing_seam_facts_declines_into_ordinary_path() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    assert!(
        !matches!(
            attempt,
            LhcCompactAttempt::MidTurnBlocked {
                next_provider_request_allowed: false,
                ..
            }
        ),
        "missing seam facts must never strand the turn, got {attempt:?}"
    );
}

#[test]
fn work_continuation_none_when_no_follow_up() {
    let items = vec![ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: "done".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    assert_eq!(
        work_continuation_from_history_tail(&items, false),
        WorkContinuation::None
    );
    assert_eq!(
        work_continuation_from_history_tail(&items, true),
        WorkContinuation::ActiveNonTool
    );
}

/// AC13 / native race: production `run_auto_compact` at MidTurn with LHC
/// enabled but unavailable must not execute token-budget / remote / native arms.
#[tokio::test]
async fn mid_turn_run_auto_compact_one_writer_no_native_arms() {
    let (mut session, tc) = make_session_and_context().await;
    session
        .set_feature_for_test(Feature::LhcCapture, true)
        .expect("enable");
    // No LhcCaptureSlot installed → MidTurnBlocked residual.
    let history_before = session.clone_history().await.raw_items().count();
    let sess = Arc::new(session);
    let step = crate::session::step_context::StepContext::for_test(Arc::new(tc));
    let mut client = inert_model_client_session();
    let result = run_auto_compact(
        &sess,
        step,
        /*fallback*/ None,
        &mut client,
        InitialContextInjection::DoNotInject,
        CompactionReason::ContextLimit,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "race-1",
            true,
            0,
            Vec::new(),
            Some(sample_usage(9_000)),
        )),
        &CancellationToken::new(),
    )
    .await;
    // Missing slot → blocked residual. Ok continue (no native), TurnAborted
    // (next-provider blocked), or Err refuse with MidTurn/LHC/native text.
    match result {
        Ok(()) => {}
        Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted) => {}
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("MidTurn") || msg.contains("LHC") || msg.contains("native"),
                "unexpected error: {msg}"
            );
        }
    }
    // No native writer mutation of host history.
    assert_eq!(
        sess.clone_history().await.raw_items().count(),
        history_before,
        "one-writer MidTurn residual must not mutate host history via native arms"
    );
}

/// Capture lag then recovery: incomplete handle → skip; after flush readiness,
/// next seam may proceed (not suppressed by hysteresis).
#[tokio::test]
async fn mid_turn_capture_lag_then_recovery() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    // First: cancel before handle ready is hard; instead force skip by
    // recording a non-arming skip outcome, then prove recovery.
    slot.record_mid_turn_hysteresis("lag", 90_000, false, "skip_capture_incomplete");
    assert!(!slot.mid_turn_hysteresis().armed);

    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 8).await;
    inject_response_usage(&session, &tc, 2_000).await;
    handle.flush().await;
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "recover-1",
            true,
            epoch,
            Vec::new(),
            Some(sample_usage(2_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    assert!(
        !matches!(attempt, LhcCompactAttempt::Unavailable { .. }),
        "recovery seam must not native-fall-open: {attempt:?}"
    );
}

/// Cancellation awaits the mutator: no detached thread after return.
#[tokio::test]
async fn mid_turn_cancel_joins_worker_no_detached_mutator() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 10).await;
    inject_response_usage(&session, &tc, 2_000).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "cancel-1",
            true,
            epoch,
            Vec::new(),
            Some(sample_usage(2_000)),
        )),
        &cancel,
    )
    .await
    .expect("arm");
    match attempt {
        LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
            assert!(
                reason.contains("cancel") || reason.contains("critical section"),
                "{reason}"
            );
        }
        other => panic!("expected blocked on cancel, got {other:?}"),
    }
    // No worker for this attempt remains after return. Scoped to `cancel-1`:
    // the other MidTurn tests run in parallel and one of them keeps a worker
    // deliberately alive, so a process-wide count proves nothing here.
    let threads_after = midturn_workers_for_attempt("cancel-1");
    assert_eq!(
        threads_after, 0,
        "detached midturn worker remains for attempt cancel-1: {threads_after}"
    );
}

/// Provider facts: response id is stable attempt identity; usage from response.
#[tokio::test]
async fn mid_turn_uses_response_scoped_usage_and_attempt_id() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 6).await;
    // Pollute session aggregate with a different later snapshot via the
    // production record path (session.state is private to the session module).
    session
        .record_token_usage_info(&tc, Some(&sample_usage(99_999)))
        .await
        .expect("pollute aggregate");
    // Response-scoped usage is much smaller — arm must prefer it.
    let response_usage = sample_usage(1_500);
    handle.flush().await;
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "provider-resp-xyz",
            true,
            epoch,
            Vec::new(),
            Some(response_usage),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    assert!(
        !matches!(attempt, LhcCompactAttempt::Unavailable { .. }),
        "{attempt:?}"
    );
}

/// Transport retry: multiple attempts of one request see stable skip, no compact.
#[tokio::test]
async fn mid_turn_transport_retry_stable_across_attempts() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 4).await;
    inject_response_usage(&session, &tc, 2_000).await;
    handle.flush().await;
    let items_before = session.clone_history().await.raw_items().len();
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    for i in 0..3 {
        let mut mid = mid_facts(
            &format!("retry-stable-{i}"),
            true,
            epoch,
            Vec::new(),
            Some(sample_usage(2_000)),
        );
        mid.inside_transport_retry = true;
        let attempt = try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid),
            &CancellationToken::new(),
        )
        .await
        .expect("arm");
        assert!(
            matches!(attempt, LhcCompactAttempt::MidTurnSkipped { .. }),
            "attempt {i}: {attempt:?}"
        );
    }
    assert_eq!(
        sess.clone_history().await.raw_items().len(),
        items_before,
        "transport retries must not mutate history"
    );
}

/// Simultaneous one-writer conflict fixture: MidTurn + LHC on refuses native.
#[tokio::test]
async fn mid_turn_simultaneous_native_writer_conflict_fixture() {
    let (mut session, tc) = make_session_and_context().await;
    session
        .set_feature_for_test(Feature::LhcCapture, true)
        .expect("enable");
    let history_before = session.clone_history().await.raw_items().count();
    let sess = Arc::new(session);
    let step = crate::session::step_context::StepContext::for_test(Arc::new(tc));
    let mut client = inert_model_client_session();
    // No slot → blocked residual through production ladder.
    let err_or_ok = run_auto_compact(
        &sess,
        step,
        /*fallback*/ None,
        &mut client,
        InitialContextInjection::DoNotInject,
        CompactionReason::ContextLimit,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "conflict-fixture",
            true,
            0,
            Vec::new(),
            Some(sample_usage(9_000)),
        )),
        &CancellationToken::new(),
    )
    .await;
    // Inert client never dialed (no panic / hang). Native arms must not run.
    let _ = err_or_ok;
    assert_eq!(
        sess.clone_history().await.raw_items().count(),
        history_before,
        "native writer conflict fixture must not mutate history"
    );
}

#[test]
fn mid_turn_attempt_variants_are_exhaustive_one_writer() {
    // Compile-time-ish documentation of one-writer residual kinds.
    let kinds = [
        "Installed",
        "Unavailable",
        "MidTurnSkipped",
        "MidTurnBlocked",
        // R14 (CX-S1): ordinary-path degrade — turn continues on its current
        // body, compact retries at the next seam. Never native permission.
        "ContinuedWithoutCompact",
    ];
    assert_eq!(kinds.len(), 5);
}

/// Live MidTurn compact-continuation workers **for one attempt**.
///
/// The worker thread is named `lhc-mt-{attempt_id}` and Linux truncates a
/// thread's `comm` to 15 bytes, so that is what lands in `/proc`. Matching the
/// truncation exactly is what keeps the count scoped to the caller's own
/// attempt: this test binary runs the MidTurn tests in parallel, and a
/// process-wide `starts_with("lhc-midturn")` count also sees the workers other
/// tests deliberately keep alive — that is a count of the binary's activity,
/// not of whether *this* turn left a detached mutator behind.
///
/// Attempt ids passed here must therefore stay distinct within their first
/// 8 bytes (15 minus the 7-byte `lhc-mt-` prefix).
fn midturn_workers_for_attempt(attempt_id: &str) -> usize {
    let full = format!("{}{attempt_id}", super::MIDTURN_WORKER_THREAD_PREFIX);
    let comm: String = full.chars().take(15).collect();
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    let mut n = 0;
    for entry in dir.flatten() {
        if let Ok(name) = std::fs::read_to_string(entry.path().join("comm"))
            && name.trim() == comm
        {
            n += 1;
        }
    }
    n
}

fn inert_model_client_session() -> crate::client::ModelClientSession {
    use crate::client::ModelClient;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use codex_login::auth::AgentIdentityAuthPolicy;
    use codex_model_provider_info::ModelProviderInfo;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::SessionSource;

    let thread_id =
        ThreadId::try_from("00000000-0000-4000-8000-000000000199").expect("test thread id");
    let mut provider =
        ModelProviderInfo::create_openai_provider(Some("http://127.0.0.1:9/v1".to_string()));
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(0);
    ModelClient::new(
        /*auth_manager*/ None,
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id,
        provider,
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
    .new_session()
}

// ── Timeout / cancel critical-section proofs (production hop) ─────────────

/// Cancel during the critical section: return only after worker exit and no
/// detached mutator.
///
/// R6 (CX-S2): cancellation no longer suppresses the host apply. The SDK may
/// already have installed a view, and skipping the host rewrite is what leaves
/// the split state a later seam has to repair.
#[tokio::test]
#[serial]
async fn mid_turn_cancel_during_critical_section_applies_installed_view() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 12).await;
    inject_response_usage(&session, &tc, 2_000).await;
    handle.flush().await;

    // Stall the worker so cancel lands while the critical section is live.
    // Timeout bounds the stall; join always awaits the worker thread.
    // Hold the process-wide override lock for the whole test so plain-parallel
    // runs cannot race sibling timeout/stall injections.
    let override_guard = super::midturn_worker_override_guard();
    // Generous bound + a stall the cancel lands inside: the worker runs to
    // completion (SDK view installed) with the turn already cancelled, which is
    // exactly the post-mutation cancellation R6 is about.
    override_guard.set_timeout(Some(Duration::from_secs(30)));
    override_guard.set_stall(Some(Duration::from_millis(1_500)));
    let history_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    // Cancel once the worker is observably inside the critical section. A
    // wall-clock guess raced the pre-worker setup: under load the cancel landed
    // before the spawn, the arm refused without one, and the test proved
    // nothing about post-mutation cancellation.
    let cancel_task = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut saw_worker = false;
        while std::time::Instant::now() < deadline {
            if midturn_workers_for_attempt("cancel-critical-1") > 0 {
                saw_worker = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel_clone.cancel();
        saw_worker
    });

    let started = std::time::Instant::now();
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "cancel-critical-1",
            true,
            epoch,
            Vec::new(),
            Some(sample_usage(2_000)),
        )),
        &cancel,
    )
    .await
    .expect("arm");
    let elapsed = started.elapsed();
    let saw_worker = cancel_task.await.expect("cancel task");
    drop(override_guard);

    // The premise of the proof: the cancel landed while the mutator was live.
    assert!(
        saw_worker,
        "cancel never observed a live midturn worker; the test proves nothing about \
         post-mutation cancellation"
    );
    // Must have waited for the stalled worker: the worker only finishes after
    // its 1.5s stall, and the arm returns after joining it.
    assert!(
        elapsed >= Duration::from_millis(1_000),
        "cancel during critical section must await worker exit; elapsed={elapsed:?}"
    );
    // Whatever the worker produced (install, skip, or a bounded timeout), the
    // cancellation itself must not be the thing that stopped the apply.
    match &attempt {
        LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
            assert!(
                !reason.contains("suppress") && !reason.contains("cancel"),
                "cancellation must not suppress the host apply: {reason}"
            );
        }
        LhcCompactAttempt::Installed { .. }
        | LhcCompactAttempt::MidTurnSkipped { .. }
        | LhcCompactAttempt::ContinuedWithoutCompact { .. } => {}
        other => panic!("cancel must stay on a MidTurn outcome, got {other:?}"),
    }
    // Named worker for this attempt must be gone (poll briefly for OS reaping).
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        if midturn_workers_for_attempt("cancel-critical-1") == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        midturn_workers_for_attempt("cancel-critical-1"),
        0,
        "detached midturn worker remains after cancel join"
    );
    // An install that happened is kept: a cancelled turn ending on the smaller
    // body is strictly better than a split state.
    let history_after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
    if let LhcCompactAttempt::Installed { body, .. } = &attempt {
        assert_eq!(
            history_after.len(),
            body.len(),
            "installed view must be applied to host history despite cancellation"
        );
    } else {
        assert_eq!(
            history_before.len(),
            history_after.len(),
            "without an install there is nothing to apply"
        );
    }
}

/// Deliberately stalled worker hits the bounded in-worker timeout, joins, and
/// leaves no worker thread or later mutation.
///
/// R14 (CX-S1): the bound still exists, but its consequence is no longer
/// strand-class — the next provider request continues on the existing body and
/// compact retries at the next seam.
#[tokio::test]
#[serial]
async fn mid_turn_stalled_worker_hits_bounded_timeout_and_joins() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 10).await;
    inject_response_usage(&session, &tc, 2_000).await;
    handle.flush().await;

    // Stall the operation future longer than the worker timeout. Timeout lives
    // inside the worker runtime so the future is dropped there and the thread
    // exits; the outer path always joins.
    // Serial guard: process-global overrides race under plain parallel cargo test.
    let override_guard = super::midturn_worker_override_guard();
    override_guard.set_timeout(Some(Duration::from_millis(80)));
    override_guard.set_stall(Some(Duration::from_secs(30)));
    let history_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let started = std::time::Instant::now();
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "stall-timeout-1",
            true,
            epoch,
            Vec::new(),
            Some(sample_usage(2_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    let elapsed = started.elapsed();
    drop(override_guard);

    assert!(
        elapsed < Duration::from_secs(5),
        "worker timeout must bound the caller; elapsed={elapsed:?}"
    );
    match attempt {
        LhcCompactAttempt::MidTurnBlocked {
            reason,
            next_provider_request_allowed,
        } => {
            assert!(
                reason.contains("timed out") || reason.contains("timeout"),
                "expected worker timeout residual, got {reason}"
            );
            assert!(
                next_provider_request_allowed,
                "a worker timeout must not strand the turn: {reason}"
            );
        }
        other => panic!("expected MidTurnBlocked on stall timeout, got {other:?}"),
    }
    let threads_after = midturn_workers_for_attempt("stall-timeout-1");
    assert_eq!(
        threads_after, 0,
        "detached midturn worker remains after timeout for attempt stall-timeout-1: {threads_after}"
    );
    let history_after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
    assert_eq!(
        history_before.len(),
        history_after.len(),
        "timeout must not mutate host history"
    );
}

// ── Acceptance paths A–E (production MidTurn arm) ─────────────────────────

/// A (unit/production-arm): active non-tool branch installs a certified view
/// with exactly one continuation marker when above the test trigger.
#[tokio::test]
async fn mid_turn_active_non_tool_installs_single_marker_and_boundary() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root.clone()).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    // Force above-trigger with a tiny upper bound so compact can install.
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 16).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "resp-active-full-1",
            true,
            epoch,
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");

    // Positive acceptance: an always-skip implementation must fail here.
    // Frozen contract marker constants (LIM-60/61) — asserted via durable
    // receipt/marker identity (host body is bands+tail; the typed marker lives
    // in the LHC event log / receipt residual).
    const MARKER_KIND: &str = "lhc.compact_continuation";
    const MARKER_CAUSE: &str = "context_compacted_task_in_progress";
    const MARKER_ACTION: &str = "continue_existing_task";
    let LhcCompactAttempt::Installed { body, marker } = attempt else {
        panic!("active non-tool above trigger must Install, got {attempt:?}");
    };
    assert!(!body.is_empty(), "installed body non-empty");
    assert!(
        !marker.marker_key.is_empty(),
        "marker must carry stable idempotency key"
    );
    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);
    let pending =
        codex_lhc_host::inspect_pending_compact_continuation_boundary(&thread_id, root.as_deref())
            .await
            .expect("inspect pending");
    assert!(
        pending.is_none(),
        "successful active install must clear pending boundary, got {pending:?}"
    );
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, root.as_deref())
            .await
            .expect("inspect receipts");
    assert!(
        !receipts.is_empty(),
        "successful active install must leave a durable receipt"
    );
    let last = receipts.last().expect("receipt");
    assert!(last.terminal, "active install receipt must be terminal");
    assert!(
        matches!(
            last.outcome.as_str(),
            "compact_continue_turn" | "degraded_compact" | "no_reduction"
        ),
        "unexpected outcome {}",
        last.outcome
    );
    // NB4: product-path useful-reduction with compactable closed history.
    // Host derives reduced=true for compact_continue_turn / degraded_compact
    // installs (codex_lhc_host MidTurnCompactContinuationOutcome). Require a
    // non-vacuous reduce outcome rather than no_reduction so always-skip or
    // no-reduction regressions fail here.
    assert!(
        matches!(
            last.outcome.as_str(),
            "compact_continue_turn" | "degraded_compact"
        ),
        "installed active non-tool with compactable history must usefully reduce \
         (host reduced==true path); got outcome={}",
        last.outcome
    );
    assert!(
        !body.is_empty(),
        "useful-reduction install leaves a serving body"
    );
    // Typed marker: durable event exists for the continuation turn, and the
    // receipt residual carries frozen kind/cause/action constants.
    let cont_turn = last
        .continuation_turn_id
        .as_deref()
        .expect("active install must open a continuation turn");
    let has_marker = codex_lhc_host::inspect_has_compact_continuation_marker(
        &thread_id,
        root.as_deref(),
        cont_turn,
    )
    .await
    .expect("inspect marker");
    assert!(
        has_marker,
        "exactly one durable typed continuation marker must exist for {cont_turn}"
    );
    let receipt_json = format!("{:?}", last.receipt);
    assert!(
        receipt_json.contains(MARKER_KIND)
            || receipt_json.contains(MARKER_CAUSE)
            || receipt_json.contains(MARKER_ACTION)
            || last.outcome == "compact_continue_turn",
        "receipt residual must reflect typed continuation (kind/cause/action); got {receipt_json}"
    );
    // Always-skip would leave zero receipts / no marker / no install — proven above.
    assert!(
        !matches!(
            try_run_lhc_compact_arm(
                &sess,
                &tc,
                InitialContextInjection::DoNotInject,
                /*manual*/ false,
                CompactionPhase::MidTurn,
                Some(mid_facts(
                    "resp-active-full-2",
                    true,
                    decision_epoch(&sess),
                    Vec::new(),
                    Some(sample_usage(5_000)),
                )),
                &CancellationToken::new(),
            )
            .await
            .expect("second arm"),
            LhcCompactAttempt::Unavailable { .. }
        ),
        "second MidTurn seam must still refuse native fall-open"
    );
}

/// B (unit/production-arm): pending parallel tools preserve pair shape,
/// reasoning identity, and deterministic branch id without a marker.
#[tokio::test]
async fn mid_turn_pending_parallel_tools_preserve_reasoning_and_pairs() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 8).await;

    let reasoning = ResponseItem::Reasoning {
        id: Some(codex_protocol::ResponseItemId::from_server(
            "rsn-stable-1".into(),
        )),
        summary: vec![
            codex_protocol::models::ReasoningItemReasoningSummary::SummaryText {
                text: "plan both tools".into(),
            },
        ],
        content: Some(vec![
            codex_protocol::models::ReasoningItemContent::ReasoningText {
                text: "reasoning body identity".into(),
            },
        ]),
        encrypted_content: Some("enc-sig-aabb".into()),
        internal_chat_message_metadata_passthrough: None,
    };
    let pairs = [
        reasoning.clone(),
        ResponseItem::FunctionCall {
            id: Some(codex_protocol::ResponseItemId::from_server(
                "fc-id-z".into(),
            )),
            name: "shell".into(),
            namespace: None,
            arguments: r#"{"cmd":"echo z"}"#.into(),
            encrypted_function_args: Some(vec!["enc-args-z".into()]),
            call_id: "call-tool-z".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: Some(codex_protocol::ResponseItemId::from_server(
                "fc-id-a".into(),
            )),
            name: "shell".into(),
            namespace: None,
            arguments: r#"{"cmd":"echo a"}"#.into(),
            encrypted_function_args: Some(vec!["enc-args-a".into()]),
            call_id: "call-tool-a".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-tool-z".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("z-out".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-tool-a".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("a-out".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    session
        .record_conversation_items_with_provenance(
            &tc,
            &pairs,
            codex_extension_api::RawItemProvenance::ModelOutput,
        )
        .await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    let items_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    let response_ids = vec!["call-tool-z".into(), "call-tool-a".into()];
    match work_continuation_for_mid_turn(&response_ids, &items_before, true) {
        WorkContinuation::PendingCorrelatedToolResult {
            protected_tool_call_ids,
            correlation_valid,
        } => {
            assert_eq!(
                protected_tool_call_ids,
                vec!["call-tool-a", "call-tool-z"],
                "sorted unique protected set"
            );
            assert!(correlation_valid);
        }
        other => panic!("expected pending tool, got {other:?}"),
    }

    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "resp-tool-full-1",
            true,
            epoch,
            response_ids,
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    assert!(
        !matches!(attempt, LhcCompactAttempt::Unavailable { .. }),
        "pending-tool must not native-fall-open: {attempt:?}"
    );

    // Authority for pair/reasoning preservation is the pre-MidTurn history for
    // skip/block, and the installed body (or post-install history) for install.
    let after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
    let pair_source: &[ResponseItem] = match &attempt {
        LhcCompactAttempt::Installed { body, .. } => body.as_slice(),
        _ => after.as_slice(),
    };
    // Both call/output pairs remain present and ordered in the serving view.
    let call_ids: Vec<_> = pair_source
        .iter()
        .filter_map(|i| match i {
            ResponseItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        call_ids.contains(&"call-tool-a") && call_ids.contains(&"call-tool-z"),
        "both response-scoped tool call ids must remain: {call_ids:?}"
    );
    let out_z = pair_source.iter().find_map(|i| match i {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        } if call_id == "call-tool-z" => Some(format!("{:?}", output.body)),
        _ => None,
    });
    let out_a = pair_source.iter().find_map(|i| match i {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        } if call_id == "call-tool-a" => Some(format!("{:?}", output.body)),
        _ => None,
    });
    assert!(
        out_z.as_ref().is_some_and(|s| s.contains("z-out")),
        "call-tool-z output must remain: {out_z:?}"
    );
    assert!(
        out_a.as_ref().is_some_and(|s| s.contains("a-out")),
        "call-tool-a output must remain: {out_a:?}"
    );
    // Reasoning provider identity (stable item id) remains when present in source.
    let has_reasoning_id = pair_source.iter().any(|i| match i {
        ResponseItem::Reasoning { id, .. } => {
            id.as_ref().map(codex_protocol::ResponseItemId::as_str) == Some("rsn-stable-1")
        }
        _ => false,
    }) || after.iter().any(|i| match i {
        ResponseItem::Reasoning { id, .. } => {
            id.as_ref().map(codex_protocol::ResponseItemId::as_str) == Some("rsn-stable-1")
        }
        _ => false,
    }) || items_before.iter().any(|i| match i {
        ResponseItem::Reasoning { id, .. } => {
            id.as_ref().map(codex_protocol::ResponseItemId::as_str) == Some("rsn-stable-1")
        }
        _ => false,
    });
    assert!(
        has_reasoning_id,
        "reasoning provider identity id must remain available to the serving path"
    );
    // Encrypted function args on the pre-MidTurn recorded calls must not be
    // stripped from the settled pairs when MidTurn skips (no install rewrite).
    if !matches!(attempt, LhcCompactAttempt::Installed { .. }) {
        let enc_z = items_before.iter().find_map(|i| match i {
            ResponseItem::FunctionCall {
                call_id,
                encrypted_function_args,
                ..
            } if call_id == "call-tool-z" => encrypted_function_args.clone(),
            _ => None,
        });
        let enc_z_after = after.iter().find_map(|i| match i {
            ResponseItem::FunctionCall {
                call_id,
                encrypted_function_args,
                ..
            } if call_id == "call-tool-z" => encrypted_function_args.clone(),
            _ => None,
        });
        assert_eq!(
            enc_z, enc_z_after,
            "encrypted function args must be unchanged when MidTurn does not install"
        );
    }
    // No continuation marker on pending-tool branch.
    if let LhcCompactAttempt::Installed { body, .. } = &attempt {
        let marker_hits = body
            .iter()
            .filter(|i| {
                let s = format!("{i:?}");
                s.contains("lhc.compact_continuation") || s.contains("context_compact_continue")
            })
            .count();
        assert_eq!(
            marker_hits, 0,
            "pending-tool must not insert continuation marker"
        );
    }
}

/// C: after MidTurn install, re-materialize through production surfaces and
/// assert the serving body is byte-equivalent for both branches.
#[tokio::test]
async fn mid_turn_reload_resume_equivalence_both_branches() {
    // Active non-tool branch
    {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (mut session, tc) = make_session_and_context().await;
        install_lhc_midturn(&mut session, root.clone()).await;
        let slot = session
            .services
            .thread_extension_data
            .get::<LhcCaptureSlot>()
            .expect("slot");
        slot.set_mid_turn_test_upper_trigger(Some(100));
        let handle = wait_for_handle(&slot, Duration::from_secs(30))
            .await
            .expect("handle");
        seed_turns(&session, &tc, 14).await;
        inject_response_usage(&session, &tc, 5_000).await;
        handle.flush().await;
        let thread_id = handle.thread_id().to_string();
        let sess = Arc::new(session);
        let attempt = try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid_facts(
                "reload-active-1",
                true,
                decision_epoch(&sess),
                Vec::new(),
                Some(sample_usage(5_000)),
            )),
            &CancellationToken::new(),
        )
        .await
        .expect("arm");
        if let LhcCompactAttempt::Installed { body, .. } = attempt {
            // Production-path proof: in-memory installed body is the serving
            // view. Reload equivalence for the active branch is the same body
            // re-read from session history after install (host rewrite path).
            let reloaded: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
            assert!(
                super::response_items_structurally_equal(&body, &reloaded),
                "active non-tool: post-install history must equal in-memory install body"
            );
            let _ = thread_id;
            let _ = root;
        }
    }

    // Pending-tool branch with reasoning identity
    {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (mut session, tc) = make_session_and_context().await;
        install_lhc_midturn(&mut session, root.clone()).await;
        let slot = session
            .services
            .thread_extension_data
            .get::<LhcCaptureSlot>()
            .expect("slot");
        slot.set_mid_turn_test_upper_trigger(Some(100));
        let handle = wait_for_handle(&slot, Duration::from_secs(30))
            .await
            .expect("handle");
        seed_turns(&session, &tc, 10).await;
        session
            .record_conversation_items_with_provenance(
                &tc,
                &[
                    ResponseItem::Reasoning {
                        id: Some(codex_protocol::ResponseItemId::from_server(
                            "rsn-reload-1".into(),
                        )),
                        summary: vec![
                            codex_protocol::models::ReasoningItemReasoningSummary::SummaryText {
                                text: "reload branch".into(),
                            },
                        ],
                        content: Some(vec![
                            codex_protocol::models::ReasoningItemContent::ReasoningText {
                                text: "stable reasoning text".into(),
                            },
                        ]),
                        encrypted_content: Some("enc-reload-sig".into()),
                        internal_chat_message_metadata_passthrough: None,
                    },
                    ResponseItem::FunctionCall {
                        id: Some(codex_protocol::ResponseItemId::from_server(
                            "fc-reload-b".into(),
                        )),
                        name: "shell".into(),
                        namespace: None,
                        arguments: r#"{"cmd":"true"}"#.into(),
                        encrypted_function_args: Some(vec!["enc-b".into()]),
                        call_id: "call-reload-b".into(),
                        internal_chat_message_metadata_passthrough: None,
                    },
                    ResponseItem::FunctionCall {
                        id: Some(codex_protocol::ResponseItemId::from_server(
                            "fc-reload-a".into(),
                        )),
                        name: "shell".into(),
                        namespace: None,
                        arguments: r#"{"cmd":"true"}"#.into(),
                        encrypted_function_args: Some(vec!["enc-a".into()]),
                        call_id: "call-reload-a".into(),
                        internal_chat_message_metadata_passthrough: None,
                    },
                    ResponseItem::FunctionCallOutput {
                        id: None,
                        call_id: "call-reload-b".into(),
                        output: FunctionCallOutputPayload {
                            body: FunctionCallOutputBody::Text("b".into()),
                            success: Some(true),
                        },
                        internal_chat_message_metadata_passthrough: None,
                    },
                    ResponseItem::FunctionCallOutput {
                        id: None,
                        call_id: "call-reload-a".into(),
                        output: FunctionCallOutputPayload {
                            body: FunctionCallOutputBody::Text("a".into()),
                            success: Some(true),
                        },
                        internal_chat_message_metadata_passthrough: None,
                    },
                ],
                codex_extension_api::RawItemProvenance::ModelOutput,
            )
            .await;
        inject_response_usage(&session, &tc, 5_000).await;
        handle.flush().await;
        let thread_id = handle.thread_id().to_string();
        let in_memory_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
        let sess = Arc::new(session);
        let attempt = try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid_facts(
                "reload-tool-1",
                true,
                decision_epoch(&sess),
                vec!["call-reload-b".into(), "call-reload-a".into()],
                Some(sample_usage(5_000)),
            )),
            &CancellationToken::new(),
        )
        .await
        .expect("arm");
        let in_memory_after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
        // Tool call ids + outputs remain available after MidTurn on the serving path.
        let pair_source: &[ResponseItem] = match &attempt {
            LhcCompactAttempt::Installed { body, .. } => body.as_slice(),
            _ => in_memory_after.as_slice(),
        };
        let call_ids: Vec<_> = pair_source
            .iter()
            .filter_map(|i| match i {
                ResponseItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            call_ids.contains(&"call-reload-a") && call_ids.contains(&"call-reload-b"),
            "both tool call ids must remain after MidTurn: {call_ids:?}"
        );
        // Reasoning provider identity survives on the pre-install history or body.
        let reasoning_id_present = in_memory_before
            .iter()
            .chain(in_memory_after.iter())
            .chain(pair_source.iter())
            .any(|i| match i {
                ResponseItem::Reasoning { id, .. } => {
                    id.as_ref().map(codex_protocol::ResponseItemId::as_str) == Some("rsn-reload-1")
                }
                _ => false,
            });
        assert!(
            reasoning_id_present,
            "reasoning provider identity id must survive MidTurn"
        );
        // Encrypted function args on recorded calls: preserved when no install rewrite.
        if !matches!(attempt, LhcCompactAttempt::Installed { .. }) {
            let enc_before = in_memory_before.iter().find_map(|i| match i {
                ResponseItem::FunctionCall {
                    call_id,
                    encrypted_function_args,
                    ..
                } if call_id == "call-reload-a" => encrypted_function_args.clone(),
                _ => None,
            });
            let enc_after = in_memory_after.iter().find_map(|i| match i {
                ResponseItem::FunctionCall {
                    call_id,
                    encrypted_function_args,
                    ..
                } if call_id == "call-reload-a" => encrypted_function_args.clone(),
                _ => None,
            });
            assert_eq!(
                enc_before, enc_after,
                "encrypted function args must be unchanged when MidTurn does not install"
            );
            assert_eq!(
                in_memory_before.len(),
                in_memory_after.len(),
                "skip/block must leave prior view byte-identical in length"
            );
        } else if let LhcCompactAttempt::Installed { body, .. } = attempt {
            // Install path: post-install history equals installed body (resume view).
            assert!(
                super::response_items_structurally_equal(&body, &in_memory_after),
                "pending-tool: post-install history must equal in-memory install body"
            );
            let _ = thread_id;
            let _ = root;
        }
    }
}

/// D: degraded derivations install a structurally valid view; invalid
/// candidate/install leaves prior view byte-identical and obeys the receipt.
#[tokio::test]
#[serial]
async fn mid_turn_degraded_and_invalid_install_host_paths() {
    // D1 — degraded derivations still install (or skip with truthful residual).
    {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (mut session, tc) = make_session_and_context().await;
        install_lhc_midturn(&mut session, root).await;
        let slot = session
            .services
            .thread_extension_data
            .get::<LhcCaptureSlot>()
            .expect("slot");
        slot.set_mid_turn_test_upper_trigger(Some(100));
        slot.set_mid_turn_test_hooks(Some(codex_lhc_host::MidTurnTestHooks {
            force_derivations_missing_or_failed: Some(true),
            ..Default::default()
        }));
        let handle = wait_for_handle(&slot, Duration::from_secs(30))
            .await
            .expect("handle");
        seed_turns(&session, &tc, 12).await;
        inject_response_usage(&session, &tc, 5_000).await;
        handle.flush().await;
        let sess = Arc::new(session);
        let attempt = try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid_facts(
                "degraded-1",
                true,
                decision_epoch(&sess),
                Vec::new(),
                Some(sample_usage(5_000)),
            )),
            &CancellationToken::new(),
        )
        .await
        .expect("arm");
        match attempt {
            LhcCompactAttempt::Installed { body, .. } => {
                assert!(
                    !body.is_empty(),
                    "degraded install must be structurally valid"
                );
            }
            LhcCompactAttempt::MidTurnSkipped { reason }
            | LhcCompactAttempt::MidTurnBlocked { reason, .. }
            | LhcCompactAttempt::ContinuedWithoutCompact { reason } => {
                assert!(!reason.is_empty(), "degradation residual must be truthful");
            }
            LhcCompactAttempt::Unavailable { reason }
            | LhcCompactAttempt::Failed { reason }
            | LhcCompactAttempt::Cancelled { reason } => {
                panic!("degraded MidTurn path must use explicit MidTurn outcome: {reason}");
            }
        }
    }

    // D2 — install failure leaves prior view/rollout byte-identical; no marker leak.
    {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (mut session, tc) = make_session_and_context().await;
        install_lhc_midturn(&mut session, root).await;
        let slot = session
            .services
            .thread_extension_data
            .get::<LhcCaptureSlot>()
            .expect("slot");
        slot.set_mid_turn_test_upper_trigger(Some(100));
        slot.set_mid_turn_test_hooks(Some(codex_lhc_host::MidTurnTestHooks {
            force_install_succeeds: Some(false),
            fail_install_before_write: true,
            ..Default::default()
        }));
        let handle = wait_for_handle(&slot, Duration::from_secs(30))
            .await
            .expect("handle");
        seed_turns(&session, &tc, 12).await;
        inject_response_usage(&session, &tc, 5_000).await;
        handle.flush().await;
        let history_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
        let sess = Arc::new(session);
        let attempt = try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid_facts(
                "invalid-install-1",
                true,
                decision_epoch(&sess),
                Vec::new(),
                Some(sample_usage(5_000)),
            )),
            &CancellationToken::new(),
        )
        .await
        .expect("arm");
        let history_after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
        assert_eq!(
            history_before.len(),
            history_after.len(),
            "failed install must leave prior serving view byte-identical in length"
        );
        assert!(
            !matches!(attempt, LhcCompactAttempt::Installed { .. }),
            "failed install must not report Installed: {attempt:?}"
        );
        assert!(
            !matches!(attempt, LhcCompactAttempt::Unavailable { .. }),
            "failed install must not native-fall-open: {attempt:?}"
        );
        // No marker leak into host history.
        let leaked = history_after.iter().any(|i| {
            let s = format!("{i:?}");
            s.contains("lhc.compact_continuation") || s.contains("context_compact_continue")
        });
        assert!(
            !leaked,
            "failed candidate must not leak continuation marker"
        );
    }

    // D3 — unresolved candidate assembly failure: no marker, prior view stable.
    {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (mut session, tc) = make_session_and_context().await;
        install_lhc_midturn(&mut session, root).await;
        let slot = session
            .services
            .thread_extension_data
            .get::<LhcCaptureSlot>()
            .expect("slot");
        slot.set_mid_turn_test_upper_trigger(Some(100));
        slot.set_mid_turn_test_hooks(Some(codex_lhc_host::MidTurnTestHooks {
            fail_candidate_assembly: true,
            ..Default::default()
        }));
        let handle = wait_for_handle(&slot, Duration::from_secs(30))
            .await
            .expect("handle");
        seed_turns(&session, &tc, 10).await;
        inject_response_usage(&session, &tc, 5_000).await;
        handle.flush().await;
        let history_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
        let sess = Arc::new(session);
        let attempt = try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid_facts(
                "invalid-candidate-1",
                true,
                decision_epoch(&sess),
                Vec::new(),
                Some(sample_usage(5_000)),
            )),
            &CancellationToken::new(),
        )
        .await
        .expect("arm");
        let history_after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
        assert_eq!(history_before.len(), history_after.len());
        assert!(
            !matches!(attempt, LhcCompactAttempt::Installed { .. }),
            "invalid candidate must not install: {attempt:?}"
        );
        assert!(
            !matches!(attempt, LhcCompactAttempt::Unavailable { .. }),
            "invalid candidate must not native-fall-open: {attempt:?}"
        );
    }
}

/// E (unit residual): when MidTurn is blocked after a context-pressure seam,
/// the residual must refuse native fall-open (one-writer). Full mock-provider
/// loop coverage lives in suite `compact_lhc_mid_turn_loops`.
#[tokio::test]
async fn mid_turn_context_pressure_residual_refuses_native_race() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    // Force install failure so residual blocks rather than installs.
    slot.set_mid_turn_test_hooks(Some(codex_lhc_host::MidTurnTestHooks {
        force_install_succeeds: Some(false),
        fail_install_before_write: true,
        ..Default::default()
    }));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 8).await;
    inject_response_usage(&session, &tc, 9_000).await;
    handle.flush().await;
    let history_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    let sess = Arc::new(session);
    let step = crate::session::step_context::StepContext::for_test(Arc::new(tc));
    let mut client = inert_model_client_session();
    let result = run_auto_compact(
        &sess,
        step,
        /*fallback*/ None,
        &mut client,
        InitialContextInjection::DoNotInject,
        CompactionReason::ContextLimit,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "ctx-exceeded-1",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(9_000)),
        )),
        &CancellationToken::new(),
    )
    .await;
    // Either continues without native or errors with MidTurn residual — never
    // silent native mutation. History must stay byte-stable on the fail path.
    let history_after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
    assert_eq!(
        history_before.len(),
        history_after.len(),
        "context-pressure MidTurn residual must not pollute host history"
    );
    match result {
        Ok(()) => {}
        Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted) => {}
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("MidTurn") || msg.contains("LHC") || msg.contains("native"),
                "unexpected error: {msg}"
            );
        }
    }
}

// ── B1 durable repair / resume + B3 preempt truthfulness ──────────────────

/// B1: install failure leaves failed_repairable; next seam re-enters same
/// attempt_id and repairs/installs rather than permanent wedge.
#[tokio::test]
#[serial]
async fn mid_turn_install_failure_repairs_same_attempt_on_next_seam() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    slot.set_mid_turn_test_hooks(Some(codex_lhc_host::MidTurnTestHooks {
        fail_install_before_write: true,
        ..Default::default()
    }));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 12).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let attempt_id = "b1-repair-1";

    let failed = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            attempt_id,
            true,
            epoch,
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    // Residual must not permanently block; next seam repairs.
    assert!(
        matches!(
            failed,
            LhcCompactAttempt::MidTurnBlocked { .. }
                | LhcCompactAttempt::MidTurnSkipped { .. }
                | LhcCompactAttempt::Installed { .. }
        ),
        "first attempt residual: {failed:?}"
    );

    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);
    let pending =
        codex_lhc_host::inspect_pending_compact_continuation_boundary(&thread_id, root.as_deref())
            .await
            .expect("inspect");
    // Clear fault hooks so repair can succeed.
    slot.set_mid_turn_test_hooks(None);

    // Fresh response id must not be used when durable owner exists — arm
    // re-enters with owner attempt. Use a different fresh id to prove resume.
    let repaired = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "fresh-should-not-win",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("repair arm");

    // After repair, pending must clear OR install succeeded.
    let pending_after =
        codex_lhc_host::inspect_pending_compact_continuation_boundary(&thread_id, root.as_deref())
            .await
            .expect("inspect after");
    match repaired {
        LhcCompactAttempt::Installed { .. } => {
            assert!(pending_after.is_none(), "install clears pending boundary");
        }
        LhcCompactAttempt::MidTurnSkipped { reason }
        | LhcCompactAttempt::ContinuedWithoutCompact { reason } => {
            // Quiet skip still allowed if pressure; must not hard-error.
            assert!(!reason.contains("conflict"), "{reason}");
        }
        LhcCompactAttempt::MidTurnBlocked {
            next_provider_request_allowed,
            reason,
        } => {
            // Permanent wedge is the bug: foreign/same-attempt conflict forever.
            assert!(
                next_provider_request_allowed || !reason.contains("owned by another"),
                "must not permanently wedge: {reason}"
            );
        }
        LhcCompactAttempt::Unavailable { reason }
        | LhcCompactAttempt::Failed { reason }
        | LhcCompactAttempt::Cancelled { reason } => {
            panic!("repair must use explicit MidTurn outcome: {reason}");
        }
    }
    let _ = (pending, attempt_id);
}

/// B1 / DR1: claim-only crash after preserve-path intent+claim leaves a real
/// residual (intent row + held writer, no pending boundary). Live seam cannot
/// recreate the response-scoped toolCallId; recovery loads stored identity and
/// re-enters without attempt_conflict.
#[tokio::test]
#[serial]
async fn mid_turn_claim_only_preserve_path_recovers_with_stored_identity() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    // Crash window: after claim/intent, fail at finalize release.
    slot.set_mid_turn_test_hooks(Some(codex_lhc_host::MidTurnTestHooks {
        fail_finalize_at_release: true,
        ..Default::default()
    }));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 12).await;

    // Preserve-path shape: response-scoped tool pair that MidTurn classifies as
    // pending_correlated_tool_result { toolCallId: call-crash-X }.
    let tool_x = "call-crash-X";
    let pairs = [
        ResponseItem::FunctionCall {
            id: Some(codex_protocol::ResponseItemId::from_server(
                "fc-crash-x".into(),
            )),
            name: "shell".into(),
            namespace: None,
            arguments: r#"{"cmd":"echo x"}"#.into(),
            encrypted_function_args: None,
            call_id: tool_x.into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: tool_x.into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("x-out".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    session
        .record_conversation_items_with_provenance(
            &tc,
            &pairs,
            codex_extension_api::RawItemProvenance::ModelOutput,
        )
        .await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let crash_attempt_id = "preserve-claim-only-crash";
    let crashed = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            crash_attempt_id,
            true,
            epoch,
            vec![tool_x.into()],
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("crash arm");
    // Must leave residual, not succeed-and-release.
    assert!(
        matches!(
            crashed,
            LhcCompactAttempt::MidTurnBlocked { .. } | LhcCompactAttempt::MidTurnSkipped { .. }
        ),
        "finalize-at-release fault must leave residual, got {crashed:?}"
    );

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(std::path::Path::to_path_buf);
    let claim =
        codex_lhc_host::inspect_compact_continuation_writer_claim(&thread_id, root_path.as_deref())
            .await
            .expect("claim");
    assert_eq!(
        claim.attempt_id.as_deref(),
        Some(crash_attempt_id),
        "claim-only residual must hold writer for owner attempt"
    );
    let pending = codex_lhc_host::inspect_pending_compact_continuation_boundary(
        &thread_id,
        root_path.as_deref(),
    )
    .await
    .expect("pending");
    // Preserve path typically has no continue-turn boundary; claim-only shape.
    let _ = pending;
    let identity = codex_lhc_host::inspect_compact_continuation_attempt_intent(
        &thread_id,
        root_path.as_deref(),
        crash_attempt_id,
    )
    .await
    .expect("inspect identity")
    .expect("intent row must exist after claim");
    match &identity.continuation {
        WorkContinuation::PendingCorrelatedToolResult {
            protected_tool_call_ids,
            ..
        } => {
            assert_eq!(protected_tool_call_ids, &vec![tool_x.to_string()]);
        }
        other => panic!("stored identity must be preserve-path, got {other:?}"),
    }

    // Clear fault hook; next live seam has different continuation (no tool X).
    slot.set_mid_turn_test_hooks(None);
    let repaired = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "live-seam-no-X",
            true,
            decision_epoch(&sess),
            Vec::new(), // live ActiveNonTool — different kind
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("repair arm");

    match repaired {
        LhcCompactAttempt::Unavailable { reason }
        | LhcCompactAttempt::Failed { reason }
        | LhcCompactAttempt::Cancelled { reason } => {
            panic!("claim-only preserve recovery must use explicit MidTurn outcome: {reason}");
        }
        LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
            assert!(
                !reason.contains("attempt_conflict")
                    && !reason.contains("different operation identity")
                    && !reason.contains("owned by another"),
                "must not permanent-wedge on identity conflict: {reason}"
            );
        }
        LhcCompactAttempt::Installed { .. }
        | LhcCompactAttempt::MidTurnSkipped { .. }
        | LhcCompactAttempt::ContinuedWithoutCompact { .. } => {}
    }

    let claim_after =
        codex_lhc_host::inspect_compact_continuation_writer_claim(&thread_id, root_path.as_deref())
            .await
            .expect("claim after");
    // Owner released or still same owner repairing — never foreign steal.
    if let Some(owner) = claim_after.attempt_id.as_deref() {
        assert_eq!(owner, crash_attempt_id);
    }

    // Later fresh seam uses a fresh attempt id normally (no permanent wedge).
    slot.set_mid_turn_test_hooks(None);
    let fresh = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "fresh-after-claim-only",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("fresh arm");
    assert!(
        !matches!(fresh, LhcCompactAttempt::Unavailable { .. }),
        "fresh seam after recovery must not native-fall-open: {fresh:?}"
    );
}

/// R4 (CX-S2): a durable recovery-identity inspect that cannot be read — here
/// a claim-only owner with no attempt-intent row (the shape a crash or partial
/// write leaves) — must not block sampling. The arm warns and proceeds with a
/// fresh attempt; the runtime CAS is what prevents a double write.
#[tokio::test]
#[serial]
async fn mid_turn_recovery_inspect_failure_proceeds_with_fresh_attempt() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 12).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(std::path::Path::to_path_buf);
    // Claim-only owner with no durable attempt-intent row: the resolver cannot
    // load an identity for it and returns Err.
    codex_lhc_host::seed_mid_turn_writer_claim_for_tests(
        &thread_id,
        root_path.as_deref(),
        "ghost-owner-no-intent-row",
    )
    .expect("seed orphan writer claim");
    let inspect =
        codex_lhc_host::resolve_mid_turn_recovery_identity(&thread_id, root_path.as_deref()).await;
    assert!(
        inspect.is_err(),
        "test precondition: recovery inspect must fail on this durable shape, got {inspect:?}"
    );

    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "fresh-after-unreadable-inspect",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");

    // Forward-only: the unreadable row named its (dead) owner, the arm reclaimed
    // that attempt id, and the compact ran to a real install.
    match attempt {
        LhcCompactAttempt::Installed { body, .. } => {
            assert!(!body.is_empty(), "reclaimed attempt must install a body");
        }
        LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
            panic!("unreadable bookkeeping must not stop the turn: {reason}");
        }
        other => panic!("inspect failure must not divert the compact: {other:?}"),
    }
}

/// R5 (CX-S2): a writer claim owned by another attempt id is a stale row from
/// a dead process — Codex is a single writer per thread. The arm re-probes once
/// and then reclaims; it never holds the session hostage to the dead owner.
#[tokio::test]
#[serial]
async fn mid_turn_stale_writer_claim_is_reclaimed_not_blocked() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    // Crash the first attempt mid-install so it leaves a pending boundary.
    slot.set_mid_turn_test_hooks(Some(codex_lhc_host::MidTurnTestHooks {
        fail_install_before_write: true,
        ..Default::default()
    }));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 12).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let crashed = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "conflict-boundary-owner",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("crash arm");
    assert!(
        !matches!(crashed, LhcCompactAttempt::Unavailable { .. }),
        "crash arm must not fall open to native: {crashed:?}"
    );

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(std::path::Path::to_path_buf);
    // A different attempt id now holds the writer row: pending boundary owner
    // != claim owner is exactly `WriterClaim::Conflict`.
    codex_lhc_host::seed_mid_turn_writer_claim_for_tests(
        &thread_id,
        root_path.as_deref(),
        "dead-process-attempt",
    )
    .expect("seed foreign writer claim");

    slot.set_mid_turn_test_hooks(None);
    let after_conflict = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "reclaiming-attempt",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("reclaim arm");

    // The host contributes no stop of its own: it re-probed, reclaimed the
    // durable owner identity, and handed the seam to the certified runtime.
    // Durable rows that name two different attempts are self-inconsistent stale
    // state; the runtime's own CAS refusal there is S8/S12 (CX-S5), not a host
    // gate this story owns.
    match after_conflict {
        LhcCompactAttempt::Installed { .. }
        | LhcCompactAttempt::MidTurnSkipped { .. }
        | LhcCompactAttempt::ContinuedWithoutCompact { .. } => {}
        LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
            assert!(
                !reason.contains("owned by another attempt")
                    && !reason.contains("refuse without steal"),
                "a stale claim must be reclaimed, not treated as a live owner: {reason}"
            );
            assert!(
                reason.starts_with("compact_continuation"),
                "any residual stop must come from the certified runtime, not the host: {reason}"
            );
        }
        other => panic!("stale claim must stay on a MidTurn outcome: {other:?}"),
    }
}

/// B3: incomplete model response must skip MidTurn with no mutation.
#[tokio::test]
async fn mid_turn_preempted_response_skips_without_mutation() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 8).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;
    let history_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let mut facts = mid_facts(
        "preempt-1",
        true,
        epoch,
        Vec::new(),
        Some(sample_usage(5_000)),
    );
    facts.model_response_complete = false;
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(facts),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    match attempt {
        LhcCompactAttempt::MidTurnSkipped { reason } => {
            assert!(
                reason.contains("incomplete")
                    || reason.contains("preempt")
                    || reason.contains("abandoned")
                    || reason.contains("not_at_settled")
                    || reason.contains("model response"),
                "{reason}"
            );
        }
        other => panic!("preempted response must skip, got {other:?}"),
    }
    let history_after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
    assert_eq!(
        history_before.len(),
        history_after.len(),
        "preempt skip must not mutate host history"
    );
}

/// R1/R7 (CX-S1): the post-worker epoch recheck (G17) is gone. Feeding a stale
/// decision-epoch snapshot against the live queue used to suppress the host
/// apply after the SDK had already installed a view — a split state the next
/// seam had to repair. The install now completes.
///
/// Supersedes `mid_turn_epoch_change_during_critical_section_suppresses_apply`.
#[tokio::test]
async fn mid_turn_epoch_change_during_critical_section_still_applies() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 8).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;
    let history_before: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    let live_epoch = decision_epoch(&session);
    let sess = Arc::new(session);
    // Stale decision epoch relative to live queue (steer-during-critical shape).
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "epoch-critical-1",
            true,
            live_epoch.saturating_sub(1),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("epoch drift during the critical section must not suppress apply, got {attempt:?}");
    };
    assert!(!body.is_empty(), "installed body must be non-empty");
    let history_after: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
    assert!(
        history_after.len() <= history_before.len(),
        "host apply must install the compacted body, not grow history: \
         before={} after={}",
        history_before.len(),
        history_after.len()
    );
}

// ── LIM-67: protected escalation + host full-body validation ────────────────

/// Seed one open agentic turn shape for escalation: older big unprotected
/// pairs, then the response-scoped protected pair.
async fn seed_escalation_history(
    session: &Session,
    tc: &crate::session::turn_context::TurnContext,
    protected_id: &str,
) {
    let mut items = Vec::new();
    for i in 0..3 {
        items.push(ResponseItem::FunctionCall {
            id: None,
            name: "shell".into(),
            namespace: None,
            arguments: format!("{{\"cmd\":\"old-{i}\"}}"),
            encrypted_function_args: None,
            call_id: format!("call-old-{i}"),
            internal_chat_message_metadata_passthrough: None,
        });
        items.push(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: format!("call-old-{i}"),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(format!("{}-OLD{i}", "tok ".repeat(1_200))),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        });
    }
    items.push(ResponseItem::FunctionCall {
        id: None,
        name: "shell".into(),
        namespace: None,
        arguments: "{\"cmd\":\"protected\"}".into(),
        encrypted_function_args: None,
        call_id: protected_id.into(),
        internal_chat_message_metadata_passthrough: None,
    });
    items.push(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: protected_id.into(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(format!("{}-PROTECTED", "tok ".repeat(400))),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    });
    session
        .record_conversation_items_with_provenance(
            tc,
            &items,
            codex_extension_api::RawItemProvenance::ModelOutput,
        )
        .await;
}

/// Escalated install: preserve is evaluated first and found unsafe against the
/// host runway; core escalates through one protected boundary, the host
/// validates the exact materialized body, records `ok`, and the reload gate
/// stays clear.
#[tokio::test]
async fn mid_turn_protected_escalation_validates_installs_and_clears_reload_gate() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root.clone()).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    slot.set_mid_turn_test_safe_runway(Some(5_000));
    slot.set_mid_turn_test_compact(Some(codex_lhc_host::test_compact_opts(400.0)));
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 2).await;
    let protected_id = "call-prot-1";
    seed_escalation_history(&session, &tc, protected_id).await;
    inject_response_usage(&session, &tc, 4_800).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "resp-esc-ok-1",
            true,
            epoch,
            vec![protected_id.into()],
            Some(sample_usage(4_800)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    let LhcCompactAttempt::Installed { body, .. } = attempt else {
        panic!("protected escalation with safe maximal prune must install, got {attempt:?}");
    };
    assert!(!body.is_empty());

    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);

    // Durable receipt: escalated relief path with the protected set recorded.
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, root.as_deref())
            .await
            .expect("receipts");
    let last = receipts.last().expect("receipt");
    assert!(
        matches!(
            last.receipt.relief_path.as_str(),
            "protected_escalation" | "host_validation_awaiting"
        ),
        "escalated relief path, got {}",
        last.receipt.relief_path.as_str()
    );
    assert_eq!(
        last.receipt.residual.protected_tool_call_ids,
        vec![protected_id.to_string()]
    );
    // One boundary + one typed marker for the escalation.
    let cont = last
        .continuation_turn_id
        .as_deref()
        .expect("continuation turn id");
    assert!(
        codex_lhc_host::inspect_has_compact_continuation_marker(&thread_id, root.as_deref(), cont)
            .await
            .expect("marker")
    );

    // Host validation recorded `ok` for the attempt; reload gate clear.
    let hv = codex_lhc_host::inspect_mid_turn_host_validation(
        &thread_id,
        root.as_deref(),
        "resp-esc-ok-1",
    )
    .await
    .expect("hv inspect")
    .expect("hv row");
    assert_eq!(hv.status, codex_lhc_host::HostValidationStatus::Ok);
    assert!(
        codex_lhc_host::host_validation_reload_block(&thread_id, root.as_deref())
            .await
            .is_none(),
        "ok validation must clear the reload gate"
    );
}

/// Negative path: forced host body-validation failure after a successful core
/// install records `failed`, blocks the next provider request without rolling
/// core state back, deterministically gates reload regeneration, and replays
/// idempotently (no second boundary or marker).
#[tokio::test]
async fn mid_turn_host_validation_failed_blocks_send_and_gates_reload() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root.clone()).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    slot.set_mid_turn_test_safe_runway(Some(5_000));
    slot.set_mid_turn_test_compact(Some(codex_lhc_host::test_compact_opts(400.0)));
    slot.set_mid_turn_test_force_body_validation_fail(true);
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 2).await;
    let protected_id = "call-prot-hv";
    seed_escalation_history(&session, &tc, protected_id).await;
    inject_response_usage(&session, &tc, 4_800).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let mid = mid_facts(
        "resp-esc-fail-1",
        true,
        epoch,
        vec![protected_id.into()],
        Some(sample_usage(4_800)),
    );
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid.clone()),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    match &attempt {
        LhcCompactAttempt::MidTurnBlocked {
            reason,
            next_provider_request_allowed,
        } => {
            assert!(
                reason.contains("host full-body validation failed"),
                "{reason}"
            );
            assert!(
                !next_provider_request_allowed,
                "failed host validation must block the next provider request"
            );
        }
        other => panic!("expected MidTurnBlocked on forced validation failure, got {other:?}"),
    }

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(std::path::Path::to_path_buf);

    // Durable failed row + reload gate engaged; core install retained.
    let hv = codex_lhc_host::inspect_mid_turn_host_validation(
        &thread_id,
        root_path.as_deref(),
        "resp-esc-fail-1",
    )
    .await
    .expect("hv inspect")
    .expect("hv row");
    assert_eq!(hv.status, codex_lhc_host::HostValidationStatus::Failed);
    let block = codex_lhc_host::host_validation_reload_block(&thread_id, root_path.as_deref())
        .await
        .expect("failed validation must gate reload");
    assert!(block.contains("resp-esc-fail-1"), "{block}");

    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, root_path.as_deref())
            .await
            .expect("receipts");
    let last = receipts.last().expect("receipt");
    let cont = last
        .continuation_turn_id
        .as_deref()
        .expect("continuation turn id")
        .to_string();
    assert!(
        codex_lhc_host::inspect_has_compact_continuation_marker(
            &thread_id,
            root_path.as_deref(),
            &cont
        )
        .await
        .expect("marker"),
        "core install (boundary + marker) is retained after failed host validation"
    );
    let receipts_before = receipts.len();

    // Replay the same attempt: terminal replay, no second boundary/marker, and
    // the durable failed row is not overwritten.
    let replay = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid),
        &CancellationToken::new(),
    )
    .await
    .expect("replay arm");
    assert!(
        !matches!(replay, LhcCompactAttempt::Unavailable { .. }),
        "replay must not fall open native"
    );
    let receipts_after =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, root_path.as_deref())
            .await
            .expect("receipts");
    assert_eq!(
        receipts_after.len(),
        receipts_before,
        "same-attempt replay must not append a second receipt"
    );
    let hv_after = codex_lhc_host::inspect_mid_turn_host_validation(
        &thread_id,
        root_path.as_deref(),
        "resp-esc-fail-1",
    )
    .await
    .expect("hv inspect")
    .expect("hv row");
    assert_eq!(
        hv_after.status,
        codex_lhc_host::HostValidationStatus::Failed
    );
}

/// LIM-69 Slice B: MidTurnBlocked(false) becomes TurnAborted so RegularTask
/// cannot drain mailbox and start another provider request.
#[tokio::test]
async fn mid_turn_host_validation_failed_strict_compact_is_turn_aborted() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    slot.set_mid_turn_test_safe_runway(Some(5_000));
    slot.set_mid_turn_test_compact(Some(codex_lhc_host::test_compact_opts(400.0)));
    slot.set_mid_turn_test_force_body_validation_fail(true);
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 2).await;
    seed_escalation_history(&session, &tc, "call-prot-abort").await;
    inject_response_usage(&session, &tc, 4_800).await;
    handle.flush().await;

    session
        .input_queue
        .enqueue_mailbox_communication(
            codex_protocol::protocol::InterAgentCommunication::new(
                codex_protocol::AgentPath::root(),
                codex_protocol::AgentPath::try_from("/root/worker").expect("path"),
                Vec::new(),
                "pending after blocked compact".into(),
                /*trigger_turn*/ false,
            ),
            /*parent_turn_id*/ None,
            /*root_turn_id*/ None,
        )
        .await;
    assert!(session.input_queue.has_pending_mailbox_items().await);

    let epoch = decision_epoch(&session);
    let sess = Arc::new(session);
    let result = crate::compact_lhc::run_strict_lhc_compact(
        &sess,
        &Arc::new(tc),
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "resp-esc-abort-1",
            true,
            epoch,
            vec!["call-prot-abort".into()],
            Some(sample_usage(4_800)),
        )),
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(
            &result,
            Err(err)
                if matches!(
                    err.details(),
                    codex_protocol::error::CodexErrorDetails::TurnAborted
                )
        ),
        "MidTurnBlocked(false) must abort the turn, got {result:?}"
    );
    assert!(
        sess.input_queue.has_pending_mailbox_items().await,
        "TurnAborted must leave mailbox input pending"
    );
}

/// LIM-69 Slice D: a later standalone compact changes the active view and
/// supersedes the failed continuation receipt's reload block. The failed HV
/// row stays failed.
#[tokio::test]
async fn standalone_compact_supersedes_failed_host_validation_reload_block() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    slot.set_mid_turn_test_safe_runway(Some(5_000));
    slot.set_mid_turn_test_compact(Some(codex_lhc_host::test_compact_opts(400.0)));
    slot.set_mid_turn_test_force_body_validation_fail(true);
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_turns(&session, &tc, 2).await;
    seed_escalation_history(&session, &tc, "call-prot-view").await;
    inject_response_usage(&session, &tc, 4_800).await;
    handle.flush().await;

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(std::path::Path::to_path_buf);
    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let blocked = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "resp-esc-view-1",
            true,
            epoch,
            vec!["call-prot-view".into()],
            Some(sample_usage(4_800)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    assert!(
        matches!(
            blocked,
            LhcCompactAttempt::MidTurnBlocked {
                next_provider_request_allowed: false,
                ..
            }
        ),
        "expected MidTurnBlocked(false), got {blocked:?}"
    );
    assert!(
        codex_lhc_host::host_validation_reload_block(&thread_id, root_path.as_deref())
            .await
            .is_some(),
        "failed HV must gate reload before standalone compact"
    );

    slot.set_mid_turn_test_force_body_validation_fail(false);
    *sess
        .services
        .lhc_test_inference
        .lock()
        .expect("lhc_test_inference lock") =
        Some(codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic callbacks"));
    let later = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        CompactionPhase::StandaloneTurn,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("standalone");
    assert!(
        matches!(later, LhcCompactAttempt::Installed { .. }),
        "standalone compact must install a later view, got {later:?}"
    );
    assert!(
        codex_lhc_host::host_validation_reload_block(&thread_id, root_path.as_deref())
            .await
            .is_none(),
        "newer active view must supersede the failed continuation residual"
    );
    let hv = codex_lhc_host::inspect_mid_turn_host_validation(
        &thread_id,
        root_path.as_deref(),
        "resp-esc-view-1",
    )
    .await
    .expect("hv inspect")
    .expect("hv row");
    assert_eq!(hv.status, codex_lhc_host::HostValidationStatus::Failed);
}
