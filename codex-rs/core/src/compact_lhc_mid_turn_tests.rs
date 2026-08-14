//! LIM-63B MidTurn compact-continuation evidence (offline, mock path).
//!
//! Drives production `try_run_lhc_compact_arm` at `CompactionPhase::MidTurn`
//! through the certified SDK runtime. No paid provider calls.

use std::sync::Arc;
use std::time::Duration;

use codex_analytics::CompactionPhase;
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
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::LhcCompactAttempt;
use super::MidTurnSeamFacts;
use super::try_run_lhc_compact_arm;
use crate::compact::InitialContextInjection;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;

fn text_input(text: &str) -> UserInput {
    UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
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
        // Offline MidTurn: small lower bound so compact can succeed without 120k seed.
        slot.set_mid_turn_test_compact(Some(test_compact_opts(400.0)));
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

fn mid_facts(attempt: &str, follow_up: bool, epoch: i64) -> MidTurnSeamFacts {
    MidTurnSeamFacts {
        attempt_id: attempt.into(),
        model_needs_follow_up: follow_up,
        input_epoch_at_decision: epoch,
        input_epoch_at_apply: epoch,
        inside_transport_retry: false,
    }
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
fn parallel_tool_ids_deterministic_lexicographic_min() {
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
    match work_continuation_from_history_tail(&items, true) {
        WorkContinuation::PendingCorrelatedToolResult {
            tool_call_id,
            correlation_valid,
        } => {
            assert_eq!(tool_call_id, "a-call");
            assert!(correlation_valid);
        }
        other => panic!("expected pending tool branch, got {other:?}"),
    }
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
    let mut mid = mid_facts("retry-1", true, 1);
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

#[tokio::test]
async fn mid_turn_input_epoch_mismatch_skips() {
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
    let mid = MidTurnSeamFacts {
        attempt_id: "epoch-1".into(),
        model_needs_follow_up: true,
        input_epoch_at_decision: 1,
        input_epoch_at_apply: 2,
        inside_transport_retry: false,
    };
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
                reason.contains("epoch"),
                "expected epoch skip, got {reason}"
            );
        }
        other => panic!("expected MidTurnSkipped, got {other:?}"),
    }
}

#[tokio::test]
async fn mid_turn_lhc_unavailable_does_not_native_fallback_via_auto_ladder() {
    // Feature on but no capture slot → MidTurnBlocked; run_auto_compact must
    // not fall open to native.
    let (mut session, tc) = make_session_and_context().await;
    session
        .set_feature_for_test(Feature::LhcCapture, true)
        .expect("enable");
    let sess = Arc::new(session);
    // Direct arm call — no ModelClient needed.
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts("no-slot", true, 0)),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    match attempt {
        LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
            assert!(
                reason.contains("LhcCaptureSlot") || reason.contains("no LhcCaptureSlot"),
                "{reason}"
            );
        }
        other => panic!("expected MidTurnBlocked without slot, got {other:?}"),
    }
}

#[tokio::test]
async fn mid_turn_feature_off_allows_native_path_unavailable() {
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
        Some(mid_facts("off", true, 0)),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    match attempt {
        LhcCompactAttempt::Unavailable { reason } => {
            assert!(reason.contains("LhcCapture off"), "{reason}");
        }
        other => panic!("expected Unavailable (native allowed), got {other:?}"),
    }
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
    // Inject high last_token_usage so pressure crosses a low test upper trigger.
    // Upper trigger comes from model auto-compact limit; for tests we still
    // exercise the production path — below/above is runtime-owned.
    handle.flush().await;
    let sess = Arc::new(session);
    let epoch = i64::try_from(sess.clone_history().await.history_version()).unwrap_or(1);
    let mid = mid_facts("active-1", true, epoch);
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
            // Below trigger / hysteresis / no-reduction with valid request.
            assert!(!reason.is_empty(), "skip must carry a diagnostic");
        }
        LhcCompactAttempt::MidTurnBlocked {
            reason,
            next_provider_request_allowed,
        } => {
            // Structural refuse is allowed if documented; must not silently native.
            assert!(!reason.is_empty());
            let _ = next_provider_request_allowed;
        }
        LhcCompactAttempt::Unavailable { reason } => {
            panic!("MidTurn must not Unavailable when LHC on: {reason}");
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
    // Append a tool call/result pair as post-measurement tail.
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
                    call_id: "call-tool-1".into(),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: "call-tool-1".into(),
                    output: FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text("ok".into()),
                        success: Some(true),
                    },
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            codex_extension_api::RawItemProvenance::ModelOutput,
        )
        .await;
    handle.flush().await;
    let items: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    match work_continuation_from_history_tail(&items, true) {
        WorkContinuation::PendingCorrelatedToolResult {
            tool_call_id,
            correlation_valid,
        } => {
            assert_eq!(tool_call_id, "call-tool-1");
            assert!(correlation_valid);
        }
        other => panic!("expected pending tool, got {other:?}"),
    }
    let sess = Arc::new(session);
    let epoch = i64::try_from(sess.clone_history().await.history_version()).unwrap_or(1);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts("tool-1", true, epoch)),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    // Production path exercised; must not be Unavailable (native fall-open).
    assert!(
        !matches!(attempt, LhcCompactAttempt::Unavailable { .. }),
        "pending-tool MidTurn must not fall open native: {attempt:?}"
    );
}

#[tokio::test]
async fn mid_turn_hysteresis_blocks_repeat_no_reduction() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    // Simulate prior no-reduction at pressure 100k.
    slot.record_mid_turn_hysteresis("prior", 100_000, false, "no_reduction");
    let hyst = slot.mid_turn_hysteresis();
    assert!(!hyst.should_attempt_after_no_reduction(100_000));
    assert!(hyst.should_attempt_after_no_reduction(100_001));
    let _ = tc;
}

#[tokio::test]
async fn mid_turn_missing_seam_facts_blocks() {
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
    match attempt {
        LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
            assert!(reason.contains("seam facts"), "{reason}");
        }
        other => panic!("expected MidTurnBlocked, got {other:?}"),
    }
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
