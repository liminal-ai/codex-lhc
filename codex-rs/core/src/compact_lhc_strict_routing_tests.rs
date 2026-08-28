//! LIM-142 strict-routing proofs for supported compact behavior that used to
//! ride native local / remote / TokenBudget fixtures.
//!
//! Production dispatch is already `run_strict_lhc_compact` for every normal
//! entry point. These tests pin the dispatch shape and the class-1 seams that
//! remain required after that routing change: PreCompact cannot veto, PostCompact
//! observes a successful install, Compact session-start is queued, and the
//! auto-compact window identity advances so current-time / remainder refresh
//! still has a new window to key off.

use std::sync::Arc;
use std::time::Duration;

use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadStartInput;
use codex_features::Feature;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::install_with_root;
use codex_lhc_host::wait_for_handle;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use codex_thread_store::PersistContext;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::LhcCompactAttempt;
use super::try_run_lhc_compact_arm_with_callbacks;
use crate::compact::InitialContextInjection;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn::run_auto_compact;
use crate::tasks::CompactTask;
use crate::tasks::SessionTask;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_lhc_host::InferenceCallbacks;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookEventName;

fn deterministic_callbacks() -> InferenceCallbacks {
    codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic offline callbacks")
}

fn install_deterministic_test_override(session: &Session) {
    *session
        .services
        .lhc_test_inference
        .lock()
        .expect("lhc_test_inference lock") = Some(deterministic_callbacks());
}

fn text_input(text: &str) -> UserInput {
    UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }
}

async fn install_lhc_and_enable(session: &mut Session, root: std::path::PathBuf) {
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
        slot.set_derivation_callbacks(deterministic_callbacks());
    }
}

async fn seed_conversation_bandable(
    session: &Session,
    tc: &crate::session::turn_context::TurnContext,
    turns: usize,
) {
    let pad = "p".repeat(2500);
    for i in 0..turns {
        let user = format!("user turn {i} long-horizon bandable seed {pad}");
        session
            .record_user_prompt_and_emit_turn_item(
                tc,
                &[text_input(&user)],
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
                        text: format!("assistant reply {i} bandable seed {pad}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                codex_extension_api::RawItemProvenance::ModelOutput,
            )
            .await;
    }
}

async fn run_arm_deterministic(
    sess: &Arc<Session>,
    tc: &crate::session::turn_context::TurnContext,
    manual: bool,
) -> LhcCompactAttempt {
    try_run_lhc_compact_arm_with_callbacks(
        sess,
        tc,
        InitialContextInjection::DoNotInject,
        manual,
        deterministic_callbacks(),
    )
    .await
    .expect("arm")
}

fn inert_model_client_session() -> crate::client::ModelClientSession {
    use crate::client::ModelClient;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use codex_login::auth::AgentIdentityAuthPolicy;
    use codex_model_provider_info::ModelProviderInfo;
    use codex_protocol::ThreadId;

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
        /*content_item_kinds_enabled*/ false,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
    .new_session()
}

fn source_file(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// CompactTask and run_auto_compact must call only `run_strict_lhc_compact`.
/// A future merge that reintroduces a TokenBudget / remote / local ladder next
/// to the strict arm fails this test.
#[test]
fn strict_dispatch_sites_have_no_native_compaction_fallback() {
    const FORBIDDEN: &[&str] = &[
        "compact_token_budget",
        "compact_remote",
        "compact_remote_v2",
        "run_compact_task",
        "run_inline_auto_compact_task",
        "run_inline_remote_auto_compact_task",
        "RemoteCompactionSupport",
        "Feature::RemoteCompactionV2",
        "Feature::TokenBudget",
    ];

    let manual = source_file("src/tasks/compact.rs");
    let auto = source_file("src/session/turn.rs");
    for (name, src) in [
        ("core/src/tasks/compact.rs", &manual),
        ("core/src/session/turn.rs", &auto),
    ] {
        assert!(
            src.contains("run_strict_lhc_compact"),
            "{name} must dispatch compaction through compact_lhc::run_strict_lhc_compact"
        );
        for needle in FORBIDDEN {
            assert!(
                !src.contains(needle),
                "{name} references `{needle}`: a native compaction fallback ladder is back \
                 next to the strict LHC arm (LIM-142)"
            );
        }
    }
    assert_eq!(
        manual.matches("run_strict_lhc_compact").count(),
        1,
        "CompactTask::run must have exactly one compaction dispatch"
    );
    assert_eq!(
        auto.matches("run_strict_lhc_compact").count(),
        1,
        "run_auto_compact must have exactly one compaction dispatch"
    );

    let arm = source_file("src/compact_lhc.rs");
    assert!(
        arm.contains("run_pre_compact_hooks") && arm.contains("run_post_compact_hooks"),
        "strict arm must keep PreCompact/PostCompact on the production path"
    );
    assert!(
        arm.contains("PreCompact hook requested stop; LHC compact continues"),
        "strict arm must keep the R15 PreCompact-stop-is-not-a-veto policy"
    );
}

/// Successful LHC install queues Compact session-start. The next turn's
/// `run_pending_session_start_hooks` is the shared producer that fires the
/// compact matcher; this is the seam hooks.rs native-compact fixtures used.
#[tokio::test]
async fn successful_lhc_compact_queues_compact_session_start() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_conversation_bandable(&session, &tc, 80).await;
    handle.flush().await;

    while session.take_pending_session_start_source().await.is_some() {}

    let sess = Arc::new(session);
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "bandable history must install: {attempt:?}"
    );
    assert!(
        matches!(
            sess.take_pending_session_start_source().await,
            Some(codex_hooks::SessionStartSource::Compact)
        ),
        "LHC install must queue Compact session-start for the next turn"
    );
}

/// Window identity advances on a successful install. Current-time reminders
/// and rollout-budget remainder restatement key off a new window id; native
/// fixtures that observed that via a summarization request are false-premise
/// here, but the window seam itself remains required.
#[tokio::test]
async fn successful_lhc_compact_advances_auto_compact_window() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_conversation_bandable(&session, &tc, 80).await;
    handle.flush().await;

    let before = session.auto_compact_window_ids().await;
    let sess = Arc::new(session);
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "bandable history must install: {attempt:?}"
    );
    let after = sess.auto_compact_window_ids().await;
    assert_eq!(after.first_window_id, before.first_window_id);
    assert_eq!(after.previous_window_id, Some(before.window_id));
    assert_ne!(
        after.window_id, before.window_id,
        "successful LHC compact must mint a new auto-compact window id"
    );
}

/// Production CompactTask still reaches the strict arm (and therefore PreCompact
/// / PostCompact) when LHC is installed. Without a capture slot the same entry
/// hard-fails and never falls open.
#[tokio::test]
async fn compact_task_installs_via_strict_arm_and_emits_post_compact_path() {
    let dir = tempdir().unwrap();
    let (sess_arc, tc_arc, rx) = make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(sess_arc) {
        Ok(session) => session,
        Err(_) => panic!("unique session"),
    };
    let tc = match Arc::try_unwrap(tc_arc) {
        Ok(tc) => tc,
        Err(_) => panic!("unique turn context"),
    };
    install_lhc_and_enable(&mut session, dir.path().to_path_buf()).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_conversation_bandable(&session, &tc, 80).await;
    handle.flush().await;
    install_deterministic_test_override(&session);

    let sess = Arc::new(session);
    let result = SessionTask::run(
        Arc::new(CompactTask),
        Arc::clone(&sess),
        Arc::new(tc),
        Vec::new(),
        CancellationToken::new(),
    )
    .await;
    assert!(
        result.is_ok(),
        "CompactTask must succeed via the strict LHC arm: {result:?}"
    );

    let mut saw_compaction_started = false;
    let mut saw_compaction_completed = false;
    while let Ok(event) = rx.try_recv() {
        match event.msg {
            EventMsg::ItemStarted(started)
                if matches!(
                    started.item,
                    codex_protocol::items::TurnItem::ContextCompaction(_)
                ) =>
            {
                saw_compaction_started = true;
            }
            EventMsg::ItemCompleted(completed)
                if matches!(
                    completed.item,
                    codex_protocol::items::TurnItem::ContextCompaction(_)
                ) =>
            {
                saw_compaction_completed = true;
            }
            EventMsg::HookStarted(started)
                if started.run.event_name == HookEventName::PreCompact
                    || started.run.event_name == HookEventName::PostCompact =>
            {
                // Optional: fixtures may have no discovered hooks.
                let _ = started;
            }
            _ => {}
        }
    }
    assert!(
        saw_compaction_started && saw_compaction_completed,
        "strict CompactTask install must emit ContextCompaction started+completed"
    );
}

/// Auto ladder (PreTurn context-limit, model-downshift, and comp-hash all
/// share this function) is the same strict arm. Native TokenBudget / remote
/// compact is not a fallback.
#[tokio::test]
async fn run_auto_compact_is_strict_lhc_only() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_conversation_bandable(&session, &tc, 80).await;
    handle.flush().await;
    install_deterministic_test_override(&session);
    while session.take_pending_session_start_source().await.is_some() {}

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
        CompactionPhase::PreTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        result.is_ok(),
        "auto ladder must complete via LHC: {result:?}"
    );
    assert!(
        matches!(
            sess.take_pending_session_start_source().await,
            Some(codex_hooks::SessionStartSource::Compact)
        ),
        "auto ladder install must queue Compact session-start"
    );
}

/// Kill-switch off is a hard failure, not native compact.
#[tokio::test]
async fn feature_off_is_hard_failure_not_native_fallback() {
    let (mut session, tc) = make_session_and_context().await;
    session
        .set_feature_for_test(Feature::LhcCapture, false)
        .expect("disable");
    let before = session.clone_history().await.into_raw_items();
    let sess = Arc::new(session);
    let result = SessionTask::run(
        Arc::new(CompactTask),
        Arc::clone(&sess),
        Arc::new(tc),
        Vec::new(),
        CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(
            &result,
            Err(err) if matches!(err.details(), CodexErrorDetails::UnsupportedOperation(_))
        ),
        "LhcCapture off must hard-fail: {result:?}"
    );
    assert_eq!(
        sess.clone_history().await.into_raw_items().len(),
        before.len(),
        "hard failure preserves history"
    );
}
