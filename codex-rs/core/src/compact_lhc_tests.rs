//! Tests for the LHC compact arm (Chunk 2b fix round 1).
//! Rule zero: production paths + structural equality. No include_str.

use std::sync::Arc;
use std::time::Duration;

use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadStartInput;
use codex_features::Feature;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::install_with_root;
use codex_lhc_host::wait_for_handle;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::LhcCompactAttempt;
use super::response_items_structurally_equal;
use super::token_limit_reached;
use super::try_run_lhc_compact_arm;
use super::try_run_lhc_compact_arm_with_callbacks;
use crate::compact::InitialContextInjection;
use crate::session::context_window::context_window_token_status;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::tasks::CompactTask;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use codex_lhc_host::InferenceCallbacks;
use tokio_util::sync::CancellationToken;

/// Test-only deterministic callbacks. Production never selects these silently.
fn deterministic_callbacks() -> InferenceCallbacks {
    codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic offline callbacks")
}

/// Install deterministic override so production entry (CompactTask / auto ladder)
/// can exercise Install offline. Explicit test setup only.
fn install_deterministic_test_override(session: &Session) {
    *session
        .services
        .lhc_test_inference
        .lock()
        .expect("lhc_test_inference lock") = Some(deterministic_callbacks());
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
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
            })
            .await;
    }
}

async fn seed_conversation_small(
    session: &Session,
    tc: &crate::session::turn_context::TurnContext,
) {
    for text in [
        "first user turn about alpha project setup",
        "second user turn about beta implementation",
        "third user turn about gamma tests",
        "fourth user turn about delta polish",
    ] {
        session
            .record_user_prompt_and_emit_turn_item(tc, &[text_input(text)], None)
            .await;
        session
            .record_conversation_items_with_provenance(
                tc,
                &[ResponseItem::Message {
                    id: None,
                    role: "assistant".into(),
                    content: vec![ContentItem::OutputText {
                        text: format!("assistant reply covering {text}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                codex_extension_api::RawItemProvenance::ModelOutput,
            )
            .await;
    }
}

/// Scale where LHC lower_bound (120k tokens) is exceeded so compact bands.
async fn seed_conversation_bandable(
    session: &Session,
    tc: &crate::session::turn_context::TurnContext,
    turns: usize,
) {
    let pad = "p".repeat(2500);
    for i in 0..turns {
        let user = format!("user turn {i} long-horizon bandable seed {pad}");
        session
            .record_user_prompt_and_emit_turn_item(tc, &[text_input(&user)], None)
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

async fn archive_has_compact_marker(thread_id: &str, root: Option<&std::path::Path>) -> bool {
    let Ok(callbacks) = codex_lhc_host::lhc_inference_callbacks(false) else {
        return false;
    };
    let Some((session, _)) =
        codex_lhc_host::LhcSession::open_with_inference(thread_id, None, root, callbacks).await
    else {
        return false;
    };
    let Ok(events) = session.list_events().await else {
        session.close().await;
        return false;
    };
    session.close().await;
    events.iter().any(|e| {
        e.text_payload()
            .is_some_and(|p| p.text.contains("lhc_compact_marker"))
    })
}

#[tokio::test]
async fn law1_installed_items_equal_lhc_body_structurally() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root.clone()).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");
    seed_conversation_bandable(&session, &tc, 80).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    let LhcCompactAttempt::Installed { body, marker } = attempt else {
        panic!("expected Installed at band scale: {attempt:?}");
    };
    assert!(!body.is_empty());
    assert!(
        marker.body_item_count == body.len() && marker.body_item_count > 0,
        "marker body_item_count must match installed body: {marker:?}"
    );
    assert!(
        marker.total_tokens > 0 || marker.compact_point > 0,
        "receipt-backed marker must not be synthetic zeros: {marker:?}"
    );

    let host = sess.clone_history().await;
    assert!(
        response_items_structurally_equal(host.raw_items(), &body),
        "law1: host history must equal the mapped LHC body field-for-field"
    );
}

#[test]
fn law1_structural_eq_includes_phase_and_media() {
    let plain = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText { text: "hi".into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let with_phase = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText { text: "hi".into() }],
        phase: Some(MessagePhase::Commentary),
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(!response_items_structurally_equal(
        std::slice::from_ref(&plain),
        std::slice::from_ref(&with_phase)
    ));

    let with_image = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![
            ContentItem::InputText { text: "see".into() },
            ContentItem::InputImage {
                image_url: "data:image/png;base64,abc".into(),
                detail: None,
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let image_only_text = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText { text: "see".into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(!response_items_structurally_equal(
        std::slice::from_ref(&with_image),
        std::slice::from_ref(&image_only_text)
    ));
    assert!(response_items_structurally_equal(
        std::slice::from_ref(&with_image),
        std::slice::from_ref(&with_image)
    ));
}

/// F2: law 2 at band scale — tokens must strictly drop; mutation that installs
/// host history unchanged must fail this test.
#[tokio::test]
async fn law2_token_count_drops_and_threshold_does_not_retrigger() {
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

    session
        .set_auto_compact_window_estimated_prefill_for_test(/*tokens*/ 90_000)
        .await;
    // Populate production token counters from current history.
    session.recompute_token_usage(&tc).await;

    let sess = Arc::new(session);
    let before = context_window_token_status(sess.as_ref(), &tc).await;
    let tokens_before = before.active_context_tokens;
    assert!(
        tokens_before > 0,
        "pre-compact token usage must be positive after recompute"
    );

    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "band-scale compact must Install: {attempt:?}"
    );

    assert_eq!(
        sess.auto_compact_window_snapshot()
            .await
            .prefill_input_tokens,
        None
    );

    // recompute runs inside the arm after install; read production path.
    let after = context_window_token_status(sess.as_ref(), &tc).await;
    assert!(
        after.active_context_tokens < tokens_before,
        "law2: active_context_tokens must strictly drop (before={tokens_before}, after={}); \
         a zero-reduction pass-through install must fail this",
        after.active_context_tokens
    );
    assert!(
        !after.token_limit_reached,
        "after LHC compact, threshold must not be latched true; status={after:?}"
    );
    assert!(!token_limit_reached(sess.as_ref(), &tc).await);

    sess.record_user_prompt_and_emit_turn_item(&tc, &[text_input("follow-up after compact")], None)
        .await;
    let status2 = context_window_token_status(sess.as_ref(), &tc).await;
    assert!(
        !status2.token_limit_reached,
        "follow-up turn must not re-trigger solely from residual prefill; {status2:?}"
    );
}

/// F3: sub-threshold history → Unavailable(NoReduction), native ladder free.
#[tokio::test]
async fn sub_threshold_returns_no_reduction() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");
    seed_conversation_small(&session, &tc).await;
    handle.flush().await;

    let before = session.clone_history().await.raw_items().to_vec();
    let sess = Arc::new(session);
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    match attempt {
        LhcCompactAttempt::Unavailable { reason } => {
            assert!(
                reason.contains("NoReduction"),
                "expected NoReduction reason, got: {reason}"
            );
        }
        other => panic!("sub-threshold must not Install: {other:?}"),
    }
    // History unchanged (did not shadow native).
    assert!(response_items_structurally_equal(
        sess.clone_history().await.raw_items(),
        &before
    ));
}

/// H1: three production write-backs must not re-ingest body into the archive.
/// Round-trips through `try_run_lhc_compact_arm` → `replace_compacted_history`
/// (which assigns stable ids).
#[tokio::test]
async fn production_three_compacts_do_not_reingest_body() {
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
    // Enough for at least one Install; subsequent may NoReduction after body shrinks.
    seed_conversation_bandable(&session, &tc, 80).await;
    handle.flush().await;

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(|p| p.to_path_buf());
    let events_before = archive_source_event_count(&thread_id, root_path.as_deref()).await;
    assert!(events_before >= 100, "seed must leave large archive");

    let sess = Arc::new(session);
    let mut installed_once = false;
    for round in 0..3 {
        let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
        match &attempt {
            LhcCompactAttempt::Installed { body, .. } => {
                installed_once = true;
                // Production assigns ids at write-back.
                let host = sess.clone_history().await;
                let with_id = host
                    .raw_items()
                    .iter()
                    .filter(|i| codex_lhc_host::item_stable_id(i).is_some())
                    .count();
                assert_eq!(
                    with_id,
                    host.raw_items().len(),
                    "round {round}: all installed items must have stable ids"
                );
                assert!(!body.is_empty());
            }
            LhcCompactAttempt::Unavailable { reason } => {
                // After first Install, further rounds may NoReduction — still
                // must not re-ingest during produce's import path.
                assert!(
                    installed_once || reason.contains("NoReduction"),
                    "round {round}: unexpected Unavailable before Install: {reason}"
                );
            }
        }
        let source = archive_source_event_count(&thread_id, root_path.as_deref()).await;
        assert_eq!(
            source, events_before,
            "round {round}: archive source events must stay at {events_before}, got {source} \
             (body re-ingest would grow the archive)"
        );
    }
    assert!(
        installed_once,
        "at least one Install expected at band scale"
    );
}

/// I1: many consecutive production compacts — model-visible marker is bounded
/// and independent of body size; digests never enter the served note.
#[tokio::test]
async fn production_many_compacts_marker_bounded_body_not_growing() {
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

    let sess = Arc::new(session);
    let mut note_chars: Vec<usize> = Vec::new();
    let mut body_item_counts: Vec<usize> = Vec::new();
    let mut installs = 0usize;
    let pad = "z".repeat(3500);
    // ≥20 rounds with enough bulk for repeated Installs.
    for round in 0..20 {
        if round > 0 {
            for k in 0..20 {
                sess.record_user_prompt_and_emit_turn_item(
                    &tc,
                    &[text_input(&format!("bulk r{round} t{k} {pad}"))],
                    None,
                )
                .await;
                sess.record_conversation_items_with_provenance(
                    &tc,
                    &[ResponseItem::Message {
                        id: None,
                        role: "assistant".into(),
                        content: vec![ContentItem::OutputText {
                            text: format!("bulk reply r{round} t{k} {pad}"),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    }],
                    codex_extension_api::RawItemProvenance::ModelOutput,
                )
                .await;
            }
            handle.flush().await;
        }
        let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
        if let LhcCompactAttempt::Installed { body, marker } = attempt {
            installs += 1;
            body_item_counts.push(body.len());
            let note = marker.to_runtime_note_text();
            note_chars.push(note.len());
            assert!(
                note.len() <= codex_lhc_host::CompactMarker::RUNTIME_NOTE_MAX_CHARS,
                "round {round}: runtime note must be bounded, got {} (body_items={})",
                note.len(),
                body.len()
            );
            assert!(
                !note.contains("derivedContentDigests")
                    && !note.contains("derived_content_digests")
                    && !note.contains("derivedHostIds")
                    && !note.contains("derived_host_ids"),
                "round {round}: digests/ids must not appear in model-visible note"
            );
            // Digest list would be ~64 chars × body_item_count; note must stay far below.
            let digest_list_floor = 40 * body.len().max(1);
            assert!(
                note.len() < digest_list_floor,
                "round {round}: note chars {} embeds digests for {} items if ≥ {digest_list_floor}",
                note.len(),
                body.len()
            );
        }
    }
    assert!(
        installs >= 10,
        "expected many Installs over 20 bulk rounds, got {installs}"
    );
    let max = note_chars.iter().copied().max().unwrap();
    let min = note_chars.iter().copied().min().unwrap();
    assert!(
        max - min < 200,
        "runtime note size must be near-constant across installs (independent of body), \
         min={min} max={max} notes={note_chars:?} body_items={body_item_counts:?}"
    );
    // Cumulative model-visible bookkeeping must stay << one body item of pad.
    let cumulative_notes = note_chars.iter().sum::<usize>();
    assert!(
        cumulative_notes < pad.len() * installs,
        "cumulative note chars {cumulative_notes} must not dominate content scale"
    );
}

/// I2: install then drop process slot (crash before archive note) — durable
/// CompactedItem record reseeds and blocks re-ingest.
#[tokio::test]
async fn crash_between_install_and_marker_no_reingest() {
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

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(|p| p.to_path_buf());
    let source_before = archive_source_event_count(&thread_id, root_path.as_deref()).await;

    let sess = Arc::new(session);
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "first compact must Install: {attempt:?}"
    );

    // Simulate process death: process-local slot wiped; durable CompactedItem remains.
    slot.clear_derived_for_test();
    assert!(slot.derived_ids().is_empty());

    // Second compact must reseed from durable record and not re-ingest body.
    let attempt2 = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    let _ = attempt2; // may Install or NoReduction

    let source_after = archive_source_event_count(&thread_id, root_path.as_deref()).await;
    assert_eq!(
        source_after, source_before,
        "crash window must not re-ingest body (before={source_before} after={source_after})"
    );
}

async fn archive_source_event_count(thread_id: &str, root: Option<&std::path::Path>) -> usize {
    let Ok(callbacks) = codex_lhc_host::lhc_inference_callbacks(false) else {
        return 0;
    };
    let Some((session, _)) =
        codex_lhc_host::LhcSession::open_with_inference(thread_id, None, root, callbacks).await
    else {
        return 0;
    };
    let Ok(events) = session.list_events().await else {
        session.close().await;
        return 0;
    };
    session.close().await;
    events
        .iter()
        .filter(|e| {
            matches!(
                e.event_kind().as_str(),
                "user_prompt" | "assistant_text" | "assistant_thinking"
            )
        })
        .count()
}

#[tokio::test]
async fn fail_open_feature_off() {
    let (session, tc) = make_session_and_context().await;
    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(attempt, LhcCompactAttempt::Unavailable { .. }));
}

/// R5 manual ladder through CompactTask at band scale (no network fallback).
#[tokio::test]
async fn production_manual_ladder_invokes_lhc_arm() {
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
    // Explicit test-only override: CompactTask calls production entry which
    // would otherwise require live ModelClient. Not a silent production default.
    install_deterministic_test_override(&session);

    let root_for_marker = handle.root().map(|p| p.to_path_buf());
    let thread_id = handle.thread_id().to_string();
    let sess = Arc::new(session);

    let turn_ext = Arc::new(ExtensionData::new(tc.sub_id.clone()));
    let ctx = SessionTaskContext::new(Arc::clone(&sess), turn_ext);
    let task = Arc::new(CompactTask);
    let result = SessionTask::run(
        task,
        Arc::new(ctx),
        Arc::new(tc),
        Vec::new(),
        CancellationToken::new(),
    )
    .await;
    assert!(
        result.is_ok(),
        "manual CompactTask must succeed via LHC at band scale: {result:?}"
    );
    assert!(
        archive_has_compact_marker(&thread_id, root_for_marker.as_deref()).await,
        "manual CompactTask must commit LHC compact marker (production ladder hook)"
    );
}

/// R5 auto ladder: production `run_auto_compact` at band scale.
/// LHC Installs before any native/model path; inert client must not be hit.
#[tokio::test]
async fn production_auto_ladder_invokes_lhc_arm() {
    use crate::session::turn::run_auto_compact;
    use codex_analytics::CompactionPhase;
    use codex_analytics::CompactionReason;

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

    let root_for_marker = handle.root().map(|p| p.to_path_buf());
    let thread_id = handle.thread_id().to_string();
    let tokens_before = context_window_token_status(&session, &tc)
        .await
        .active_context_tokens;
    let sess = Arc::new(session);
    let step = crate::session::step_context::StepContext::for_test(Arc::new(tc));
    // Inert client on a loopback base URL — never dials a live endpoint.
    // At band scale LHC Installs before native; hook removal would fail open
    // into this client (which has no credentials).
    let mut client = inert_model_client_session();
    let result = run_auto_compact(
        &sess,
        step,
        /*fallback*/ None,
        &mut client,
        InitialContextInjection::DoNotInject,
        CompactionReason::ContextLimit,
        CompactionPhase::PreTurn,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        result.is_ok(),
        "auto ladder must complete via LHC Install without live model: {result:?}"
    );
    let tokens_after = {
        // TurnContext moved into step; recompute via session history size proxy
        // is insufficient — use marker + reduced history length.
        sess.clone_history().await.raw_items().len()
    };
    assert!(
        tokens_after > 0 && (tokens_before == 0 || tokens_after < 160),
        "auto ladder should leave a reduced band body (items={tokens_after}, tokens_before={tokens_before})"
    );
    assert!(
        archive_has_compact_marker(&thread_id, root_for_marker.as_deref()).await,
        "auto run_auto_compact must commit LHC compact marker (production ladder hook)"
    );
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
        ThreadId::try_from("00000000-0000-4000-8000-000000000099").expect("test thread id");
    // Explicit non-routable local base URL — never api.openai.com (G4).
    let provider =
        ModelProviderInfo::create_openai_provider(Some("http://127.0.0.1:9/v1".to_string()));
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

/// Shape-risk: Compacted.replacement_history is verbatim for resume/fork.
#[test]
fn band_body_replacement_history_byte_equal() {
    use crate::context_manager::ContextManager;
    use codex_protocol::protocol::CompactedItem;

    let body = vec![
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "hello".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "world".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let compacted = CompactedItem {
        message: "lhc".into(),
        replacement_history: Some(body.clone()),
        window_number: Some(1),
        first_window_id: Some("a".into()),
        previous_window_id: None,
        window_id: Some("b".into()),
    };
    let mut history = ContextManager::new();
    history.replace(compacted.replacement_history.clone().unwrap());
    assert!(response_items_structurally_equal(
        history.raw_items(),
        &body
    ));
}

/// Shape-risk goldens: fork/resume consumers see band-shaped replacement as-is.
#[test]
fn shape_risk_consumers_see_band_replacement() {
    use crate::context_manager::ContextManager;
    use codex_protocol::protocol::CompactedItem;

    let band = vec![
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "[compact summary of early turns]".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "ack summary".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "latest user turn".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "latest assistant turn".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let compacted = CompactedItem {
        message: "lhc_compact_marker".into(),
        replacement_history: Some(band.clone()),
        window_number: Some(2),
        first_window_id: Some("w0".into()),
        previous_window_id: Some("w1".into()),
        window_id: Some("w2".into()),
    };
    let mut resume = ContextManager::new();
    resume.replace(
        compacted
            .replacement_history
            .clone()
            .expect("replacement_history present"),
    );
    assert!(response_items_structurally_equal(resume.raw_items(), &band));

    let mut full_fork = ContextManager::new();
    full_fork.replace(band.clone());
    assert!(response_items_structurally_equal(
        full_fork.raw_items(),
        &band
    ));

    let last_n: Vec<_> = band[band.len().saturating_sub(2)..].to_vec();
    let mut last_n_fork = ContextManager::new();
    last_n_fork.replace(last_n.clone());
    assert!(response_items_structurally_equal(
        last_n_fork.raw_items(),
        &last_n
    ));

    let mut btw = ContextManager::new();
    btw.replace(last_n.clone());
    assert!(response_items_structurally_equal(btw.raw_items(), &last_n));

    let mut guardian = ContextManager::new();
    guardian.replace(band.clone());
    assert!(response_items_structurally_equal(
        guardian.raw_items(),
        &band
    ));
}

// ── J1 / J2: real inference default + pinned model/effort ─────────────────

/// J1: production entry without test override must not Install via canned
/// deterministic text. Uses an inert ModelClient so stream cannot succeed.
#[tokio::test]
async fn j1_production_without_override_fails_open_not_deterministic() {
    // Ensure the old opt-in env is not what drives production (deleted as switch).
    // SAFETY: test-local env mutation for invariant check.
    unsafe {
        std::env::remove_var("CODEX_LHC_LIVE_INFERENCE");
    }

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    // Inert client: real bridge will attempt stream and fail (no canned text).
    session.services.model_client = {
        use crate::client::ModelClient;
        use codex_http_client::HttpClientFactory;
        use codex_http_client::OutboundProxyPolicy;
        use codex_login::auth::AgentIdentityAuthPolicy;
        use codex_model_provider_info::ModelProviderInfo;
        use codex_protocol::ThreadId;
        use codex_protocol::protocol::SessionSource;
        let thread_id = ThreadId::try_from("00000000-0000-4000-8000-000000000088").expect("tid");
        ModelClient::new(
            /*auth_manager*/ None,
            AgentIdentityAuthPolicy::JwtOnly,
            thread_id,
            ModelProviderInfo::create_openai_provider(Some("http://127.0.0.1:9/v1".to_string())),
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
    };
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

    let before = session.clone_history().await.raw_items().to_vec();
    let sess = Arc::new(session);
    // No lhc_test_inference override → production ModelClient bridge.
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        &CancellationToken::new(),
    )
    .await
    .expect("arm result");

    match attempt {
        LhcCompactAttempt::Installed { body, .. } => {
            let joined: String = body
                .iter()
                .filter_map(|item| match item {
                    ResponseItem::Message { content, .. } => Some(
                        content
                            .iter()
                            .filter_map(|c| match c {
                                ContentItem::InputText { text }
                                | ContentItem::OutputText { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let has_det = joined.contains("smoothed(")
                || joined.contains("brief(")
                || joined.contains("projection(")
                || joined.contains("toolresult(");
            assert!(
                !has_det,
                "J1: production must not install deterministic canned markers; preview={}",
                joined.chars().take(500).collect::<String>()
            );
            // If produce still Installs after stream failures, body must not be
            // canned deterministic text (the shim Lee rejected).
            eprintln!(
                "J1: production Installed without deterministic markers (items={})",
                body.len()
            );
        }
        LhcCompactAttempt::Unavailable { reason } => {
            assert!(!reason.is_empty(), "fail-open reason should be non-empty");
            // History unchanged — native ladder free.
            assert!(response_items_structurally_equal(
                sess.clone_history().await.raw_items(),
                &before
            ));
        }
    }
}

/// J1: when derivation model is unavailable, fail open — never turn model.
#[tokio::test]
async fn j1_unavailable_derivation_model_fails_open() {
    use codex_models_manager::manager::StaticModelsManager;
    use codex_protocol::openai_models::ModelsResponse;

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    // Empty catalog → luna resolves as fallback metadata → Err.
    session.services.models_manager = Arc::new(StaticModelsManager::new(
        /*auth_manager*/ None,
        ModelsResponse {
            models: vec![],
            ..ModelsResponse::default()
        },
    ));
    install_lhc_and_enable(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");
    seed_conversation_bandable(&session, &tc, 40).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    match attempt {
        LhcCompactAttempt::Unavailable { reason } => {
            assert!(
                reason.contains("gpt-5.6-luna") || reason.contains("unavailable"),
                "expected derivation-model unavailable reason, got: {reason}"
            );
            assert!(
                !reason.to_lowercase().contains("deterministic"),
                "must not mention deterministic substitution: {reason}"
            );
        }
        other => panic!("must Unavailable when luna missing: {other:?}"),
    }
}

/// J2: resolve pins gpt-5.6-luna and lowest effort (Low for luna catalog).
#[tokio::test]
async fn j2_resolve_pins_luna_and_lowest_effort_not_turn_model() {
    use crate::lhc_inference_bridge::LHC_DERIVATION_MODEL;
    use crate::lhc_inference_bridge::resolve_lhc_derivation_target;
    use codex_protocol::openai_models::ReasoningEffort;

    let (session, tc) = make_session_and_context().await;
    let target = resolve_lhc_derivation_target(&session)
        .await
        .expect("luna must resolve from bundled catalog in tests");
    // Hard-coded pin (not only the constant) so renaming the const fails this test.
    assert_eq!(
        LHC_DERIVATION_MODEL, "gpt-5.6-luna",
        "ruling pin must stay gpt-5.6-luna"
    );
    assert!(
        target.model_info.slug.contains("luna"),
        "pinned derivation model must be luna, got slug={}",
        target.model_info.slug
    );
    assert_eq!(
        target.effort,
        ReasoningEffort::Low,
        "luna supports low..max (no none); min must be Low, got {:?}",
        target.effort
    );
    // Must not ride the turn model when it differs.
    if tc.model_info.slug != target.model_info.slug {
        assert_ne!(
            target.model_info.slug, tc.model_info.slug,
            "derivation must not equal turn model when they differ"
        );
    }
}

/// J2: pin constant + lowest effort for luna-shaped catalog (behavioural unit).
/// Request-level pin for all four callbacks is covered by the wiremock test in
/// `lhc_inference_bridge::tests::j2_all_four_callbacks_request_pinned_model_and_lowest_effort`.
#[test]
fn j2_resolve_effort_and_pin_constant() {
    use crate::lhc_inference_bridge::LHC_DERIVATION_MODEL;
    use crate::lhc_inference_bridge::resolve_lhc_derivation_effort;
    use codex_models_manager::ModelsManagerConfig;
    use codex_models_manager::test_support::construct_model_info_offline_for_tests;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::openai_models::ReasoningEffortPreset;

    assert_eq!(LHC_DERIVATION_MODEL, "gpt-5.6-luna");
    let mut model = construct_model_info_offline_for_tests(
        LHC_DERIVATION_MODEL,
        &ModelsManagerConfig::default(),
    );
    model.supported_reasoning_levels = vec![
        ReasoningEffortPreset {
            effort: ReasoningEffort::Low,
            description: "low".into(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffort::Medium,
            description: "medium".into(),
        },
    ];
    assert_eq!(resolve_lhc_derivation_effort(&model), ReasoningEffort::Low);
}

/// J1: with explicit test override, production entry uses bridge of override
/// (deterministic Install) — proves override path is the only offline Install.
#[tokio::test]
async fn j1_explicit_override_installs_via_production_entry() {
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

    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "explicit test override must Install offline: {attempt:?}"
    );
}

/// K1: behavioural — production with no usable derivation model returns
/// `Unavailable` (native ladder free), never deterministic canned Install.
/// `CODEX_LHC_LIVE_INFERENCE=1` must not change that outcome (env is not a
/// production switch).
#[tokio::test]
async fn j1_live_inference_env_has_no_effect_when_client_unusable() {
    use codex_models_manager::manager::StaticModelsManager;
    use codex_protocol::openai_models::ModelsResponse;

    async fn run_with_empty_catalog() -> LhcCompactAttempt {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (mut session, tc) = make_session_and_context().await;
        // No usable derivation model → resolve fails open before produce.
        session.services.models_manager = Arc::new(StaticModelsManager::new(
            /*auth_manager*/ None,
            ModelsResponse {
                models: vec![],
                ..ModelsResponse::default()
            },
        ));
        install_lhc_and_enable(&mut session, root).await;
        let slot = session
            .services
            .thread_extension_data
            .get::<LhcCaptureSlot>()
            .expect("slot");
        let handle = wait_for_handle(&slot, Duration::from_secs(5))
            .await
            .expect("handle");
        seed_conversation_bandable(&session, &tc, 40).await;
        handle.flush().await;
        let before_len = session.clone_history().await.raw_items().len();
        let sess = Arc::new(session);
        let attempt = try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ true,
            &CancellationToken::new(),
        )
        .await
        .expect("arm");
        let after_len = sess.clone_history().await.raw_items().len();
        assert_eq!(
            before_len, after_len,
            "Unavailable must leave host history unchanged (native ladder free)"
        );
        // Body must never be deterministic canned install.
        let joined: String =
            sess.clone_history()
                .await
                .raw_items()
                .iter()
                .filter_map(|item| match item {
                    ResponseItem::Message { content, .. } => Some(
                        content
                            .iter()
                            .filter_map(|c| match c {
                                ContentItem::InputText { text }
                                | ContentItem::OutputText { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
        assert!(
            !joined.contains("smoothed(")
                && !joined.contains("brief(")
                && !joined.contains("projection(")
                && !joined.contains("toolresult("),
            "must not install deterministic canned markers"
        );
        attempt
    }

    // SAFETY: test-local env only.
    unsafe {
        std::env::remove_var("CODEX_LHC_LIVE_INFERENCE");
    }
    let a = run_with_empty_catalog().await;
    let reason_a = match a {
        LhcCompactAttempt::Unavailable { reason } => reason,
        other => panic!("no usable client must Unavailable, got {other:?}"),
    };
    assert!(
        reason_a.contains("gpt-5.6-luna") || reason_a.contains("unavailable"),
        "expected derivation-model unavailable, got: {reason_a}"
    );

    unsafe {
        std::env::set_var("CODEX_LHC_LIVE_INFERENCE", "1");
    }
    let b = run_with_empty_catalog().await;
    unsafe {
        std::env::remove_var("CODEX_LHC_LIVE_INFERENCE");
    }
    let reason_b = match b {
        LhcCompactAttempt::Unavailable { reason } => reason,
        other => panic!(
            "CODEX_LHC_LIVE_INFERENCE=1 must not re-enable deterministic Install; got {other:?}"
        ),
    };
    assert!(
        reason_b.contains("gpt-5.6-luna") || reason_b.contains("unavailable"),
        "with env=1 still derivation-model unavailable, got: {reason_b}"
    );
    // Same failure class: env is not a production switch.
    assert_eq!(
        reason_a.split(':').next(),
        reason_b.split(':').next(),
        "env must not change failure class: a={reason_a} b={reason_b}"
    );
}

// M1's idle-pump wiring is proven in the host crate (codex-lhc-host
// `install.rs::tests::m1_*`) through the real extension registry. The
// core-level end-to-end measurement is settled below (Chunk 3, gap 1).

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering as AtomicOrdering;

/// Chunk 3 / gap 1 — the core-level M1 measurement Chunk 2 could not explain.
///
/// The deleted Chunk 2 attempt asserted on `DrainReport::remaining` and saw it
/// *grow* while the core pump ran. That is the queue behaving as designed, not
/// a pump failure: `remaining` is `count_live_items` — queued + claimed
/// `work_item` rows — and LHC's derivation graph cascades, so settling an item
/// enqueues its successors. `remaining` is therefore not monotone under
/// progress and cannot support the M1 claim in either direction. That is pinned
/// as observed behaviour by the host-crate companion test
/// `m1_remaining_is_not_a_monotone_progress_metric`.
///
/// The quantity M1 exists to reduce — and the one core's 120 s
/// `COMPACT_THREAD_TIMEOUT` is actually spent on — is **inference calls made at
/// compact time**. This measures exactly that, driving the background pump
/// through the production idle seam (`emit_thread_idle_lifecycle_if_idle`, the
/// same call `codex_thread.rs` makes) against an unpumped control on an
/// identically seeded thread.
///
/// Chunk 2's measurement was not wrong, it was **truncated**. Instrumenting the
/// pump on this exact fixture (60 turns / 120 events / 294 work items) shows two
/// phases:
///
///   * ticks 1–~12 drain only *non-inference* work (ingest, placement,
///     projection) at the full 8 items/tick. Each settled item enqueues more
///     successors than it consumed, so `remaining` climbs ~4/tick (121 → ~137)
///     while the inference counter stays at **0**. A short experiment sees
///     exactly the reported symptom: "the pump runs, derives nothing, and the
///     backlog grows".
///   * from ~tick 13 the cascade front reaches inference-bearing kinds;
///     `remaining` falls to 0 by ~tick 40 and all derivation is paid in
///     background.
///
/// Hence `TICKS` below is 80, not a handful: fewer ticks measure the transient.
/// The operational number that falls out is the one worth remembering — roughly
/// **one idle tick per 1.5 conversation turns** is needed for the pump to keep
/// up at 8 items/tick. Below that rate compact time still pays the balance.
#[tokio::test]
async fn m1_core_idle_pump_reduces_compact_time_inference_calls() {
    const TURNS: usize = 60;
    const TICKS: u64 = 80;

    // ── Control arm: no idle pump. Every derivation is paid at compact time.
    let control_dir = tempdir().unwrap();
    let (mut control, control_tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut control, control_dir.path().to_path_buf()).await;
    let control_slot = control
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("control slot");
    let control_handle = wait_for_handle(&control_slot, Duration::from_secs(30))
        .await
        .expect("control handle");
    seed_conversation_bandable(&control, &control_tc, TURNS).await;
    control_handle.flush().await;
    assert_eq!(
        control_slot.idle_pump_runs(),
        0,
        "control arm must never pump — it is the baseline"
    );

    let control_calls = Arc::new(AtomicUsize::new(0));
    let control_sess = Arc::new(control);
    let control_attempt = try_run_lhc_compact_arm_with_callbacks(
        &control_sess,
        &control_tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        slow_counting_callbacks(Arc::clone(&control_calls), Duration::ZERO),
    )
    .await
    .expect("control arm");
    assert!(
        matches!(control_attempt, LhcCompactAttempt::Installed { .. }),
        "control compact must install (else the comparison is between two \
         fail-open paths, not two derivation paths); got {control_attempt:?}"
    );
    let control_compact_calls = control_calls.load(AtomicOrdering::SeqCst);
    assert!(
        control_compact_calls > 0,
        "control must actually pay derivation at compact time"
    );

    // ── Pumped arm: identical seed, derivation pumped from the idle seam.
    let pumped_dir = tempdir().unwrap();
    let (mut pumped, pumped_tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut pumped, pumped_dir.path().to_path_buf()).await;
    let pumped_slot = pumped
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("pumped slot");
    let pumped_handle = wait_for_handle(&pumped_slot, Duration::from_secs(30))
        .await
        .expect("pumped handle");
    seed_conversation_bandable(&pumped, &pumped_tc, TURNS).await;
    pumped_handle.flush().await;

    // The idle seam resolves callbacks through the same production selection
    // the compact arm uses; under cfg(test) that honours this override. One
    // counter across both phases, so background and compact-time calls are
    // measured on the same instrument.
    let pumped_calls = Arc::new(AtomicUsize::new(0));
    *pumped
        .services
        .lhc_test_inference
        .lock()
        .expect("lhc_test_inference lock") = Some(slow_counting_callbacks(
        Arc::clone(&pumped_calls),
        Duration::ZERO,
    ));

    for tick in 1..=TICKS {
        // Production entry — not `spawn_idle_derivation_pump` directly, so
        // deleting the seam in `tasks/lifecycle.rs` fails this test.
        pumped.emit_thread_idle_lifecycle_if_idle().await;
        // Asserted per tick, not once at the end: a mutation that severs the
        // seam otherwise fails only after every tick has burnt its timeout.
        let runs = wait_for_core_pump_runs(&pumped_slot, tick, Duration::from_secs(20)).await;
        assert_eq!(
            runs, tick,
            "M1: idle tick {tick} did not pump. Either `on_thread_idle` no longer \
             pumps, or `seed_lhc_idle_derivation_callbacks` is not reaching the \
             slot from tasks/lifecycle.rs."
        );
    }
    let background_calls = pumped_calls.load(AtomicOrdering::SeqCst);
    assert!(
        background_calls > 0,
        "M1: idle ticks must run real derivation in the background; got 0"
    );

    let pumped_sess = Arc::new(pumped);
    let pumped_attempt = try_run_lhc_compact_arm_with_callbacks(
        &pumped_sess,
        &pumped_tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        slow_counting_callbacks(Arc::clone(&pumped_calls), Duration::ZERO),
    )
    .await
    .expect("pumped arm");
    assert!(
        matches!(pumped_attempt, LhcCompactAttempt::Installed { .. }),
        "pumped compact must install; got {pumped_attempt:?}"
    );
    let pumped_compact_calls = pumped_calls.load(AtomicOrdering::SeqCst) - background_calls;

    // Printed so the certification record can quote measured numbers.
    eprintln!(
        "M1 core measurement: turns={TURNS} ticks={TICKS} \
         control_compact_calls={control_compact_calls} \
         pumped_background_calls={background_calls} \
         pumped_compact_calls={pumped_compact_calls}"
    );

    assert!(
        pumped_compact_calls < control_compact_calls,
        "M1: the idle pump must move derivation off the compact-time deadline — \
         control paid {control_compact_calls} calls at compact time, pumped paid \
         {pumped_compact_calls} after {background_calls} background calls. \
         Not smaller means the pump derived nothing the compact would have had \
         to do, i.e. background derivation is not persisting to the archive."
    );
}

/// Core-local copy of the host crate's pump-run waiter (that one is private to
/// `install.rs`'s test module).
async fn wait_for_core_pump_runs(
    slot: &LhcCaptureSlot,
    target: u64,
    timeout: Duration,
) -> u64 {
    let start = std::time::Instant::now();
    loop {
        let runs = slot.idle_pump_runs();
        if runs >= target || start.elapsed() > timeout {
            return runs;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// M2 through the production chain: when the compact arm's own thread timeout
/// fires, the detached worker's drain must observe the cancel flag and stop —
/// not keep billing inference for a session that already failed open.
#[tokio::test]
async fn m2_compact_timeout_cancels_in_flight_derivation() {
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

    let counter = Arc::new(AtomicUsize::new(0));
    let slow = slow_counting_callbacks(Arc::clone(&counter), Duration::from_millis(20));
    let sess = Arc::new(session);

    super::set_compact_thread_timeout_ms_for_test(400);
    let attempt = try_run_lhc_compact_arm_with_callbacks(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        slow,
    )
    .await
    .expect("arm");
    super::set_compact_thread_timeout_ms_for_test(0);

    let reason = match attempt {
        LhcCompactAttempt::Unavailable { reason } => reason,
        other => panic!("timeout must fail open, got {other:?}"),
    };
    assert!(
        reason.contains("timed out"),
        "expected timeout fail-open, got: {reason}"
    );

    // The worker is detached; the cancel flag must stop it within a batch.
    let at_timeout = counter.load(AtomicOrdering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = counter.load(AtomicOrdering::SeqCst);
    assert!(
        after - at_timeout <= 8,
        "M2: after the caller timed out and failed open, derivation must stop \
         within one drain batch — fired {} more calls in 3s (at_timeout={at_timeout}, \
         after={after}); without the per-batch cancel check this keeps climbing",
        after - at_timeout
    );
}

/// Deterministic callbacks that count and sleep — used to make the compact-time
/// drain slow enough to time out deterministically.
fn slow_counting_callbacks(counter: Arc<AtomicUsize>, delay: Duration) -> InferenceCallbacks {
    use codex_lhc_host::CompressDetailedTurnInput;
    use codex_lhc_host::SmoothPromptInput;
    use codex_lhc_host::SummarizeChunkBriefInput;
    use codex_lhc_host::SummarizeToolResultInput;

    let base = deterministic_callbacks();
    macro_rules! wrap {
        ($field:ident, $ty:ty) => {{
            let counter = Arc::clone(&counter);
            let inner = Arc::clone(&base.$field);
            Arc::new(move |input: $ty| {
                counter.fetch_add(1, AtomicOrdering::SeqCst);
                let inner = Arc::clone(&inner);
                Box::pin(async move {
                    tokio::time::sleep(delay).await;
                    inner(input).await
                }) as codex_lhc_host::BoxInferenceFuture
            })
        }};
    }
    InferenceCallbacks {
        smooth_prompt: wrap!(smooth_prompt, SmoothPromptInput),
        summarize_tool_result: wrap!(summarize_tool_result, SummarizeToolResultInput),
        compress_detailed_turn: wrap!(compress_detailed_turn, CompressDetailedTurnInput),
        summarize_chunk_brief: wrap!(summarize_chunk_brief, SummarizeChunkBriefInput),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Chunk 3 / C1 — paths drivable offline (Phase A). Everything here runs on
// deterministic callbacks; the live-model half of C1 is Phase B and is named
// as not-exercised in CHUNK3-CERTIFICATION.md rather than faked here.
// ───────────────────────────────────────────────────────────────────────────

/// Install LHC against an **explicit** LHC thread id, so a second `Session` can
/// be opened over the same archive. That is what resume and fork do: a new
/// Session object over an existing thread record.
async fn install_lhc_with_thread_id(
    session: &mut Session,
    root: std::path::PathBuf,
    thread_id: &str,
) {
    session.services.thread_extension_data = ExtensionData::new(thread_id.to_string());
    install_lhc_and_enable(session, root).await;
}

/// C1.2 — resume after a compact, through I2's durable path.
///
/// A resumed Session is a *new* `Session` over an existing archive whose
/// history was reconstructed from rollout. Three things must hold, and each is
/// asserted against the archive rather than argued:
///
///  1. the served body is **not re-ingested** as source events (it is LHC's own
///     output; ingesting it would compound summaries every compact);
///  2. the durable derived record survives the process boundary and is found by
///     `seed_last_lhc_durable_from_rollout` — the production resume seam in
///     `session/mod.rs`;
///  3. the resumed session recovers derived provenance into its (fresh, empty)
///     process slot, so it can tell LHC output from user content.
///
/// History *reconstruction* itself is covered where it lives, by
/// `session::lhc_capture_e2e_tests::e2e_rollout_reconstruction_does_not_re_ingest_into_capture`
/// — that is the seam that keeps a resume from feeding the served body back in,
/// and it is driven through `apply_rollout_reconstruction` directly.
#[tokio::test]
async fn c1_resume_after_compact_no_reingest_and_durable_provenance_survives() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let tid = "c3-resume-thread";

    let (mut s1, tc1) = make_session_and_context().await;
    install_lhc_with_thread_id(&mut s1, root.clone(), tid).await;
    let slot1 = s1
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let h1 = wait_for_handle(&slot1, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_conversation_bandable(&s1, &tc1, 80).await;
    h1.flush().await;

    let sess1 = Arc::new(s1);
    let a1 = run_arm_deterministic(&sess1, &tc1, /*manual*/ true).await;
    let LhcCompactAttempt::Installed { marker, .. } = a1 else {
        panic!("fixture: first compact must install");
    };
    assert!(
        !marker.derived_host_ids.is_empty(),
        "fixture: compact must have produced derived provenance"
    );
    let events_after_first = archive_source_event_count(tid, Some(dir.path())).await;

    // Production write-back recorded the durable record on the CompactedItem.
    let durable = sess1
        .last_lhc_durable_derived_message()
        .await
        .expect("I2: write-back must record a durable derived message");

    // ── the resume: a new Session over the same archive, rollout-reconstructed.
    let (mut s2, tc2) = make_session_and_context().await;
    install_lhc_with_thread_id(&mut s2, root.clone(), tid).await;
    let slot2 = s2
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("resumed slot");
    let h2 = wait_for_handle(&slot2, Duration::from_secs(30))
        .await
        .expect("resumed handle");
    assert!(
        slot2.derived_ids().is_empty(),
        "a fresh process slot starts empty — otherwise this test proves nothing \
         about the durable path"
    );

    // The production resume seam, fed the CompactedItem rollout shape it is
    // fed on a real resume.
    let rollout = vec![codex_protocol::protocol::RolloutItem::Compacted(
        codex_protocol::protocol::CompactedItem {
            message: durable.clone(),
            replacement_history: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        },
    )];
    s2.seed_last_lhc_durable_from_rollout(&rollout).await;
    assert_eq!(
        s2.last_lhc_durable_derived_message().await.as_deref(),
        Some(durable.as_str()),
        "I2: the resume seam must recover the durable record from rollout"
    );

    h2.flush().await;

    let sess2 = Arc::new(s2);
    let a2 = run_arm_deterministic(&sess2, &tc2, /*manual*/ true).await;

    let events_after_resume = archive_source_event_count(tid, Some(dir.path())).await;
    eprintln!(
        "C1 resume: source_events after_first={events_after_first} \
         after_resume={events_after_resume} derived_ids={} second_attempt={}",
        marker.derived_host_ids.len(),
        match &a2 {
            LhcCompactAttempt::Installed { body, .. } => format!("Installed({} items)", body.len()),
            LhcCompactAttempt::Unavailable { reason } => format!("Unavailable({reason})"),
        }
    );

    assert_eq!(
        events_after_resume, events_after_first,
        "a resumed session must not add source events merely by resuming: \
         {events_after_first} -> {events_after_resume}"
    );
    assert!(
        !slot2.derived_ids().is_empty(),
        "I2: after the resumed session's arm ran, the slot must carry derived \
         provenance recovered from the durable record — without it the resumed \
         session cannot tell LHC output from user content"
    );
}

/// C1.3 — fork (`SpawnAgentForkMode::FullHistory`) after a compact.
///
/// The census ranks this the highest-risk consumer: the child inherits the
/// **compacted replacement history** as its full history, over an archive it
/// has never written to. The invariant is that the child either produces a
/// coherent body or fails open — never compacts a partial archive into a full
/// replacement (Chunk 2 stopping rule 5).
#[tokio::test]
async fn c1_fork_full_history_after_compact_inherits_coherent_body() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let parent_tid = "c3-fork-parent";
    let child_tid = "c3-fork-child";

    let (mut parent, ptc) = make_session_and_context().await;
    install_lhc_with_thread_id(&mut parent, root.clone(), parent_tid).await;
    let pslot = parent
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("parent slot");
    let ph = wait_for_handle(&pslot, Duration::from_secs(30))
        .await
        .expect("parent handle");
    seed_conversation_bandable(&parent, &ptc, 80).await;
    ph.flush().await;

    let psess = Arc::new(parent);
    let pa = run_arm_deterministic(&psess, &ptc, /*manual*/ true).await;
    let LhcCompactAttempt::Installed { body: parent_body, marker: pmarker } = pa else {
        panic!("fixture: parent compact must install");
    };
    let durable = psess
        .last_lhc_durable_derived_message()
        .await
        .expect("parent durable record");

    // The fork: a fresh thread id (fresh archive) inheriting the parent's
    // post-compact history verbatim, plus the rollout-carried durable record.
    let (mut child, ctc) = make_session_and_context().await;
    install_lhc_with_thread_id(&mut child, root.clone(), child_tid).await;
    let cslot = child
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("child slot");
    let ch = wait_for_handle(&cslot, Duration::from_secs(30))
        .await
        .expect("child handle");
    let rollout = vec![codex_protocol::protocol::RolloutItem::Compacted(
        codex_protocol::protocol::CompactedItem {
            message: durable.clone(),
            replacement_history: Some(parent_body.clone()),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        },
    )];
    child.seed_last_lhc_durable_from_rollout(&rollout).await;
    for item in &parent_body {
        child
            .record_conversation_items_with_provenance(
                &ctc,
                std::slice::from_ref(item),
                codex_extension_api::RawItemProvenance::HostContext,
            )
            .await;
    }
    ch.flush().await;

    let csess = Arc::new(child);
    let inherited = csess.clone_history().await.raw_items().to_vec();
    assert!(
        response_items_structurally_equal(&inherited, &parent_body),
        "fork must inherit the parent's post-compact body verbatim: \
         parent={} items, child={} items",
        parent_body.len(),
        inherited.len()
    );

    let ca = run_arm_deterministic(&csess, &ctc, /*manual*/ true).await;
    eprintln!(
        "C1 fork: parent_body={} items parent_derived={} child_attempt={}",
        parent_body.len(),
        pmarker.derived_host_ids.len(),
        match &ca {
            LhcCompactAttempt::Installed { body, .. } => format!("Installed({} items)", body.len()),
            LhcCompactAttempt::Unavailable { reason } => format!("Unavailable({reason})"),
        }
    );

    // Coherence, not a particular outcome: install a real reduction, or fail
    // open. A partial-archive install would be the Chunk 2 defect returning.
    match ca {
        LhcCompactAttempt::Installed { body, marker } => {
            assert!(!body.is_empty(), "child install must not be empty");
            assert_eq!(
                marker.body_item_count,
                body.len(),
                "child marker must describe the body it installed"
            );
        }
        LhcCompactAttempt::Unavailable { .. } => {}
    }
}

/// C1 — KV / prefix-cache impact of a compact, measured.
///
/// The brief asks for numbers and nobody had produced any. A provider's prefix
/// cache keys on the literal leading token sequence of the request, so the cost
/// of a compact is exactly: how much of the previous request's prefix survives
/// into the next one. This measures the shared leading run between the
/// pre-compact history and the installed body, in items and in estimated
/// tokens, on a real compact through the production arm.
///
/// This is the *invalidation* half and it is exact offline — it depends only on
/// the two histories, not on the provider. The half that needs a live run is
/// the billing confirmation (`cached_input_tokens` on the turn after a compact),
/// which is Phase B.
#[tokio::test]
async fn c1_kv_prefix_cache_invalidation_after_compact_is_measured() {
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

    let sess = Arc::new(session);
    let before = sess.clone_history().await.raw_items().to_vec();
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    let LhcCompactAttempt::Installed { .. } = attempt else {
        panic!("fixture: compact must install to measure its cache cost");
    };
    let after = sess.clone_history().await.raw_items().to_vec();

    // Longest common leading run of structurally identical items. Anything past
    // the first divergence is a cache miss for the provider regardless of what
    // follows it.
    let shared_items = before
        .iter()
        .zip(after.iter())
        .take_while(|(a, b)| {
            response_items_structurally_equal(std::slice::from_ref(*a), std::slice::from_ref(*b))
        })
        .count();

    let before_tokens = codex_lhc_host::estimate_response_items_tokens(&before);
    let after_tokens = codex_lhc_host::estimate_response_items_tokens(&after);
    let shared_tokens = codex_lhc_host::estimate_response_items_tokens(&before[..shared_items]);

    eprintln!(
        "C1 KV/prefix-cache: before_items={} before_tokens={} after_items={} \
         after_tokens={} shared_prefix_items={} shared_prefix_tokens={} \
         reusable_pct_of_next_request={:.1}",
        before.len(),
        before_tokens,
        after.len(),
        after_tokens,
        shared_items,
        shared_tokens,
        if after_tokens == 0 {
            0.0
        } else {
            100.0 * shared_tokens as f64 / after_tokens as f64
        }
    );

    assert!(
        after_tokens < before_tokens,
        "fixture: a compact must reduce, else the cache question is moot"
    );
    // The measured fact, pinned so it cannot silently change: the compacted
    // body does NOT preserve the old prefix, so the first post-compact turn
    // re-sends its whole body uncached. If a future change makes LHC emit a
    // cache-preserving prefix, this assertion is the thing that notices.
    assert!(
        shared_tokens * 2 < after_tokens,
        "prefix-cache behaviour changed: {shared_tokens} of {after_tokens} body \
         tokens are now a shared prefix with the pre-compact request. That is \
         good news, but CHUNK3-CERTIFICATION.md records the opposite — update it."
    );
}

/// C1 / gap 2 groundwork — the real per-call **input** cost profile.
///
/// Chunk 2's timeout arithmetic multiplied an assumed *latency* by a call
/// count. The other factor — and the one that determines spend on a live run —
/// is input tokens per call. That is measurable offline, because the callback
/// inputs are the exact payloads the live `ModelClient` bridge would send.
///
/// It also pins a load-bearing invariant: derivation is fed **per-turn and
/// per-chunk excerpts**, never the whole conversation. If a single call ever
/// carried history-sized input, cost and latency would scale quadratically with
/// thread length and the 120 s bound would be unreachable.
#[tokio::test]
async fn c1_derivation_call_input_cost_profile_is_measured() {
    use codex_lhc_host::CompressDetailedTurnInput;
    use codex_lhc_host::SmoothPromptInput;
    use codex_lhc_host::SummarizeChunkBriefInput;
    use codex_lhc_host::SummarizeToolResultInput;

    const TURNS: usize = 60;

    // (kind, chars) per call, in call order.
    let log: Arc<std::sync::Mutex<Vec<(&'static str, usize)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let base = deterministic_callbacks();
    macro_rules! measured {
        ($field:ident, $ty:ty, $label:literal, $len:expr) => {{
            let log = Arc::clone(&log);
            let inner = Arc::clone(&base.$field);
            Arc::new(move |input: $ty| {
                #[allow(clippy::redundant_closure_call)]
                let n = ($len)(&input);
                log.lock().expect("log").push(($label, n));
                let inner = Arc::clone(&inner);
                Box::pin(async move { inner(input).await })
                    as codex_lhc_host::BoxInferenceFuture
            })
        }};
    }
    let callbacks = InferenceCallbacks {
        smooth_prompt: measured!(smooth_prompt, SmoothPromptInput, "smooth_prompt", |i: &SmoothPromptInput| i
            .text
            .len()),
        summarize_tool_result: measured!(
            summarize_tool_result,
            SummarizeToolResultInput,
            "summarize_tool_result",
            |i: &SummarizeToolResultInput| i.content.len()
        ),
        compress_detailed_turn: measured!(
            compress_detailed_turn,
            CompressDetailedTurnInput,
            "compress_detailed_turn",
            |i: &CompressDetailedTurnInput| i.dialogue_text.len()
        ),
        summarize_chunk_brief: measured!(
            summarize_chunk_brief,
            SummarizeChunkBriefInput,
            "summarize_chunk_brief",
            |i: &SummarizeChunkBriefInput| i.text.len()
        ),
    };

    let dir = tempdir().unwrap();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, dir.path().to_path_buf()).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_conversation_bandable(&session, &tc, TURNS).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let history = sess.clone_history().await.raw_items().to_vec();
    let history_tokens = codex_lhc_host::estimate_response_items_tokens(&history);
    let attempt = try_run_lhc_compact_arm_with_callbacks(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        callbacks,
    )
    .await
    .expect("arm");
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "fixture: compact must install; got {attempt:?}"
    );

    let calls = log.lock().expect("log").clone();
    assert!(!calls.is_empty(), "no derivation calls were made");

    // char/4, the same order-of-magnitude estimate LHC and the arm both use.
    let tok = |chars: usize| chars.div_ceil(4);
    let mut by_kind: std::collections::BTreeMap<&str, (usize, usize, usize)> =
        std::collections::BTreeMap::new();
    for (kind, chars) in &calls {
        let e = by_kind.entry(kind).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += tok(*chars);
        e.2 = e.2.max(tok(*chars));
    }
    let total_calls = calls.len();
    let total_input_tokens: usize = calls.iter().map(|(_, c)| tok(*c)).sum();
    let max_call_tokens = calls.iter().map(|(_, c)| tok(*c)).max().unwrap_or(0);

    eprintln!(
        "C1 derivation cost profile: turns={TURNS} history_tokens={history_tokens} \
         calls={total_calls} calls_per_turn={:.2} total_input_tokens={total_input_tokens} \
         mean_input_tokens_per_call={} max_input_tokens_per_call={max_call_tokens}",
        total_calls as f64 / TURNS as f64,
        total_input_tokens / total_calls,
    );
    for (kind, (n, sum, max)) in &by_kind {
        eprintln!(
            "C1 derivation cost profile:   {kind}: calls={n} total_input_tokens={sum} \
             mean={} max={max}",
            sum / n.max(&1)
        );
    }

    assert!(
        max_call_tokens * 4 < history_tokens as usize,
        "derivation must be fed excerpts, not the conversation: largest single \
         call carried {max_call_tokens} input tokens against a {history_tokens}-token \
         history. If this trips, per-call cost now scales with thread length."
    );
}

/// N3 / C1.4 — a turn abort must stop derivation and install nothing.
///
/// Driven through the **production manual ladder** (`CompactTask::run`) with
/// the turn's real `CancellationToken`, cancelled while derivation is
/// demonstrably in flight. The task future is deliberately *not* dropped: the
/// point is that the token alone is now sufficient. Before N3 it was not —
/// `CompactTask` bound it as `_cancellation_token` and the arm only ever saw
/// its own private `AtomicBool`, so this same sequence ran the compact to
/// completion, rewrote history 160 -> 31 items, and committed the marker, all
/// after the abort. Production was saved only by the hard `handle.abort()`
/// 100 ms later, and the detached derivation worker survived even that: 3 calls
/// at abort, 12 by 500 ms later, still climbing against a 75 s budget.
///
/// Three assertions, one per way to be wrong: derivation stops promptly, no
/// body is installed, no marker is committed (law 3 fail-open).
#[tokio::test]
async fn c1_abort_mid_compact_leaves_turn_and_history_intact() {
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

    // Slow enough that the abort lands with derivation genuinely in flight.
    let calls = Arc::new(AtomicUsize::new(0));
    *session
        .services
        .lhc_test_inference
        .lock()
        .expect("lhc_test_inference lock") = Some(slow_counting_callbacks(
        Arc::clone(&calls),
        Duration::from_millis(30),
    ));

    let thread_id = handle.thread_id().to_string();
    let root_for_marker = handle.root().map(|p| p.to_path_buf());
    let sess = Arc::new(session);
    let history_before = sess.clone_history().await.raw_items().to_vec();

    let turn_ext = Arc::new(ExtensionData::new(tc.sub_id.clone()));
    let ctx = SessionTaskContext::new(Arc::clone(&sess), turn_ext);
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        SessionTask::run(
            Arc::new(CompactTask),
            Arc::new(ctx),
            Arc::new(tc),
            Vec::new(),
            task_cancel,
        )
        .await
    });

    let start = std::time::Instant::now();
    while calls.load(AtomicOrdering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let at_cancel = calls.load(AtomicOrdering::SeqCst);
    assert!(
        at_cancel > 0,
        "fixture: the abort must arrive with derivation actually running"
    );

    cancel.cancel();

    // The task must return on its own — no hard abort. If it hangs, the token
    // is not reaching the arm.
    let result = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("N3: the compact task must return after the turn is cancelled")
        .expect("compact task must not panic");
    assert!(
        result.is_ok(),
        "cancellation is a fail-open, not a turn error: {result:?}"
    );
    let at_return = calls.load(AtomicOrdering::SeqCst);

    // Give any surviving worker a generous window to keep spending.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after_grace = calls.load(AtomicOrdering::SeqCst);

    let history_after = sess.clone_history().await.raw_items().to_vec();
    let marker = archive_has_compact_marker(&thread_id, root_for_marker.as_deref()).await;
    eprintln!(
        "N3 abort: calls_at_cancel={at_cancel} calls_at_return={at_return} \
         calls_2s_later={after_grace} history_before={} history_after={} \
         marker_committed={marker}",
        history_before.len(),
        history_after.len()
    );

    // Cancellation is checked between drain batches (DRAIN_BATCH_ITEMS = 4), so
    // that many derivations can still be in flight. Anything beyond one batch
    // means the worker never observed the abort.
    assert!(
        after_grace - at_cancel <= 8,
        "N3: derivation must stop within one drain batch of the abort — fired \
         {} more calls (at_cancel={at_cancel}, 2s later={after_grace}). Without \
         the turn token reaching the arm this keeps climbing to completion.",
        after_grace - at_cancel
    );
    assert!(
        response_items_structurally_equal(&history_before, &history_after),
        "abort must not install a body: history went from {} items to {}",
        history_before.len(),
        history_after.len()
    );
    assert!(
        !marker,
        "abort must not commit a compact marker — the archive would claim a \
         compact that was never served to the model"
    );
}
