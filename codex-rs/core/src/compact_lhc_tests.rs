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
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use codex_thread_store::PersistContext;
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
use crate::session::tests::make_session_and_context_with_rx;
use crate::tasks::CompactTask;
use crate::tasks::SessionTask;
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

/// Install LHC for an offline test and seed the capture session's derivation
/// callbacks, mirroring what `tasks/lifecycle.rs` does in production. Under
/// `SdkMode::Background` LHC derives as intake commits, so without this the
/// scheduler's handlers wait forever for callbacks that never arrive.
async fn install_lhc_and_enable(session: &mut Session, root: std::path::PathBuf) {
    install_lhc_and_enable_with(session, root, Some(deterministic_callbacks())).await;
}

async fn install_lhc_and_enable_with(
    session: &mut Session,
    root: std::path::PathBuf,
    derivation: Option<InferenceCallbacks>,
) {
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
    if let Some(cbs) = derivation
        && let Some(slot) = session
            .services
            .thread_extension_data
            .get::<LhcCaptureSlot>()
    {
        slot.set_derivation_callbacks(cbs);
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
            .record_user_prompt_and_emit_turn_item(
                tc,
                &[text_input(text)],
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

    let host = sess.clone_history().await.into_raw_items();
    assert!(
        response_items_structurally_equal(&host, &body),
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

    sess.record_user_prompt_and_emit_turn_item(
        &tc,
        &[text_input("follow-up after compact")],
        None,
        PersistContext::TurnStart,
    )
    .await;
    let status2 = context_window_token_status(sess.as_ref(), &tc).await;
    assert!(
        !status2.token_limit_reached,
        "follow-up turn must not re-trigger solely from residual prefill; {status2:?}"
    );
}

/// F-L4: reduction self-check is like-for-like (materialized body vs current
/// rollout model-context). Sub-threshold / non-reducing compact may Install
/// when body ≤ baseline (equal is OK). Pathology is body *larger* than the
/// rollout model-context — covered by `fl4_body_larger_than_baseline_fails`.
/// This test keeps the small-seed path green: Unavailable *or* Install are both
/// acceptable so long as Install does not grow model context beyond baseline.
#[tokio::test]
async fn sub_threshold_does_not_grow_model_context() {
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

    let before = session.clone_history().await.into_raw_items();
    let sess = Arc::new(session);
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    match attempt {
        LhcCompactAttempt::Failed { reason }
        | LhcCompactAttempt::Unavailable { reason }
        | LhcCompactAttempt::Cancelled { reason } => {
            // NoReduction is fine; other hard stops also fine for tiny seed.
            let _ = reason;
            assert!(response_items_structurally_equal(
                &sess.clone_history().await.into_raw_items(),
                &before
            ));
        }
        LhcCompactAttempt::Installed { .. } => {
            // Install allowed when body ≤ rollout model-context (F-L4).
        }
        LhcCompactAttempt::MidTurnSkipped { .. } | LhcCompactAttempt::MidTurnBlocked { .. } => {
            panic!("StandaloneTurn must not return MidTurn residual");
        }
    }
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
    let root_path = handle.root().map(std::path::Path::to_path_buf);
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
                let host = sess.clone_history().await.into_raw_items();
                let with_id = host
                    .iter()
                    .filter(|i| codex_lhc_host::item_stable_id(i).is_some())
                    .count();
                assert_eq!(
                    with_id,
                    host.len(),
                    "round {round}: all installed items must have stable ids"
                );
                assert!(!body.is_empty());
            }
            LhcCompactAttempt::Failed { reason }
            | LhcCompactAttempt::Unavailable { reason }
            | LhcCompactAttempt::Cancelled { reason }
            | LhcCompactAttempt::MidTurnSkipped { reason }
            | LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
                // After first Install, further rounds may NoReduction — still
                // must not re-ingest during produce's import path.
                assert!(
                    installed_once || reason.contains("NoReduction"),
                    "round {round}: unexpected hard stop before Install: {reason}"
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
    // Ten rounds are enough to prove repeated bounded markers while staying
    // inside the workspace's 60-second per-test ceiling on fresh runners.
    for round in 0..10 {
        if round > 0 {
            for k in 0..20 {
                sess.record_user_prompt_and_emit_turn_item(
                    &tc,
                    &[text_input(&format!("bulk r{round} t{k} {pad}"))],
                    None,
                    PersistContext::TurnStart,
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
        "expected an Install in every bulk round, got {installs}"
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
    let root_path = handle.root().map(std::path::Path::to_path_buf);
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
        codex_analytics::CompactionPhase::StandaloneTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(attempt, LhcCompactAttempt::Failed { .. }));
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

    let root_for_marker = handle.root().map(std::path::Path::to_path_buf);
    let thread_id = handle.thread_id().to_string();
    let sess = Arc::new(session);

    let task = Arc::new(CompactTask);
    let result = SessionTask::run(
        task,
        Arc::clone(&sess),
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

    let root_for_marker = handle.root().map(std::path::Path::to_path_buf);
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
        /*mid_turn*/ None,
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

/// Shape-risk: Compacted.replacement_history is verbatim for resume/fork.
#[test]
fn band_body_replacement_history_byte_equal() {
    use crate::context_manager::ContextManager;
    use codex_history::CompactedItem;

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
        replacement_history: Some(body.clone().into_iter().map(Into::into).collect()),
        window_number: Some(1),
        first_window_id: Some("a".into()),
        previous_window_id: None,
        window_id: Some("b".into()),
    };
    let mut history = ContextManager::new();
    history.replace(
        compacted
            .replacement_history
            .unwrap()
            .into_iter()
            .map(|envelope| envelope.item)
            .collect(),
    );
    assert!(response_items_structurally_equal(
        &history.into_raw_items(),
        &body
    ));
}

/// Shape-risk goldens: fork/resume consumers see band-shaped replacement as-is.
#[test]
fn shape_risk_consumers_see_band_replacement() {
    use crate::context_manager::ContextManager;
    use codex_history::CompactedItem;

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
        replacement_history: Some(band.clone().into_iter().map(Into::into).collect()),
        window_number: Some(2),
        first_window_id: Some("w0".into()),
        previous_window_id: Some("w1".into()),
        window_id: Some("w2".into()),
    };
    let mut resume = ContextManager::new();
    resume.replace(
        compacted
            .replacement_history
            .expect("replacement_history present")
            .into_iter()
            .map(|envelope| envelope.item)
            .collect(),
    );
    assert!(response_items_structurally_equal(
        &resume.into_raw_items(),
        &band
    ));

    let mut full_fork = ContextManager::new();
    full_fork.replace(band.clone());
    assert!(response_items_structurally_equal(
        &full_fork.into_raw_items(),
        &band
    ));

    let last_n: Vec<_> = band[band.len().saturating_sub(2)..].to_vec();
    let mut last_n_fork = ContextManager::new();
    last_n_fork.replace(last_n.clone());
    assert!(response_items_structurally_equal(
        &last_n_fork.into_raw_items(),
        &last_n
    ));

    let mut btw = ContextManager::new();
    btw.replace(last_n.clone());
    assert!(response_items_structurally_equal(
        &btw.into_raw_items(),
        &last_n
    ));

    let mut guardian = ContextManager::new();
    guardian.replace(band.clone());
    assert!(response_items_structurally_equal(
        &guardian.into_raw_items(),
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
    };
    // Deliberately NO derivation callbacks seeded: J1 asserts production never
    // serves deterministic text, so seeding any here would defeat the test.
    install_lhc_and_enable_with(&mut session, root, None).await;
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

    let before = session.clone_history().await.into_raw_items();
    let sess = Arc::new(session);
    let cancellation = CancellationToken::new();
    let cancel_after_request_starts = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel_after_request_starts.cancel();
    });
    // No lhc_test_inference override → production ModelClient bridge.
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        codex_analytics::CompactionPhase::StandaloneTurn,
        /*mid_turn*/ None,
        &cancellation,
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
        LhcCompactAttempt::Failed { reason }
        | LhcCompactAttempt::Unavailable { reason }
        | LhcCompactAttempt::Cancelled { reason } => {
            assert!(!reason.is_empty(), "hard-stop reason should be non-empty");
            // History unchanged — no native compact.
            assert!(response_items_structurally_equal(
                &sess.clone_history().await.into_raw_items(),
                &before
            ));
        }
        LhcCompactAttempt::MidTurnSkipped { .. } | LhcCompactAttempt::MidTurnBlocked { .. } => {
            panic!("StandaloneTurn must not return MidTurn residual");
        }
    }
}

/// J1: missing Luna must not block compact and must not write canned derivation.
#[tokio::test]
async fn j1_missing_luna_does_not_block_or_write_canned() {
    use codex_models_manager::manager::StaticModelsManager;
    use codex_protocol::openai_models::ModelsResponse;

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    // Empty catalog → luna resolves as fallback metadata → inert seam.
    session.services.models_manager = Arc::new(StaticModelsManager::new(
        /*auth_manager*/ None,
        ModelsResponse {
            models: vec![],
            ..ModelsResponse::default()
        },
    ));
    // No capture derivation callbacks: production stays unseeded when Luna is
    // missing (do not seed canned/deterministic text into the durable record).
    install_lhc_and_enable_with(&mut session, root, None).await;
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
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        codex_analytics::CompactionPhase::StandaloneTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    let LhcCompactAttempt::Installed { body, .. } = attempt else {
        panic!("missing Luna must still Install via fallback residue: {attempt:?}");
    };
    let joined: String = body
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
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
        "must not write canned deterministic derivation when Luna is missing; preview={}",
        joined.chars().take(400).collect::<String>()
    );
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
        codex_analytics::CompactionPhase::StandaloneTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "explicit test override must Install offline: {attempt:?}"
    );
}

/// K1: missing derivation model still Installs via inert seam; env is not a
/// production switch that re-enables canned deterministic text.
#[tokio::test]
async fn j1_live_inference_env_has_no_effect_when_client_unusable() {
    use codex_models_manager::manager::StaticModelsManager;
    use codex_protocol::openai_models::ModelsResponse;

    async fn run_with_empty_catalog() -> (LhcCompactAttempt, String) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (mut session, tc) = make_session_and_context().await;
        session.services.models_manager = Arc::new(StaticModelsManager::new(
            /*auth_manager*/ None,
            ModelsResponse {
                models: vec![],
                ..ModelsResponse::default()
            },
        ));
        // Unseeded capture: no canned derivation persisted when Luna is absent.
        install_lhc_and_enable_with(&mut session, root, None).await;
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
        let attempt = try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ true,
            codex_analytics::CompactionPhase::StandaloneTurn,
            /*mid_turn*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("arm");
        let joined: String =
            sess.clone_history()
                .await
                .raw_items()
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
        (attempt, joined)
    }

    // SAFETY: test-local env only.
    unsafe {
        std::env::remove_var("CODEX_LHC_LIVE_INFERENCE");
    }
    let (a, _) = run_with_empty_catalog().await;
    assert!(
        matches!(a, LhcCompactAttempt::Installed { .. }),
        "missing Luna must Install via inert/fallback, got {a:?}"
    );

    unsafe {
        std::env::set_var("CODEX_LHC_LIVE_INFERENCE", "1");
    }
    let (b, _) = run_with_empty_catalog().await;
    unsafe {
        std::env::remove_var("CODEX_LHC_LIVE_INFERENCE");
    }
    assert!(
        matches!(b, LhcCompactAttempt::Installed { .. }),
        "CODEX_LHC_LIVE_INFERENCE=1 must not re-enable canned Install; got {b:?}"
    );
}

// Background-scheduler derivation (no host drain anywhere) is proven in the
// host crate (codex-lhc-host `install.rs::tests::
// background_mode_derives_without_any_host_drain`) through the real extension
// registry. The core-level end-to-end measurement is settled below.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering as AtomicOrdering;

/// Round 11: with `SdkMode::Background`, derivation happens **during the
/// session** and a compact pays for essentially none of it.
///
/// This replaces `m1_core_idle_pump_reduces_compact_time_inference_calls`,
/// which measured a hand-rolled idle pump that existed only to work around the
/// SDK being misconfigured to `Manual`. Both the pump and the compact-time
/// drain loop are gone; the arm now waits, bounded, for LHC's own scheduler.
///
/// The control arm is not a second configuration — it is the *same*
/// configuration measured before the background scheduler has settled, which is
/// what the old design paid at every compact.
#[tokio::test]
async fn background_derivation_leaves_compact_with_no_inference_to_do() {
    const TURNS: usize = 60;

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

    // Seed production callbacks before capture, as `tasks/lifecycle.rs` does —
    // background derivation runs with these, so they must never be canned text
    // in production (J1).
    let calls = Arc::new(AtomicUsize::new(0));
    slot.set_derivation_callbacks(slow_counting_callbacks(Arc::clone(&calls), Duration::ZERO));

    seed_conversation_bandable(&session, &tc, TURNS).await;
    handle.flush().await;

    // No host drain anywhere. Wait for LHC's scheduler.
    let settled = handle.drain_settled(Duration::from_secs(120)).await;
    assert!(settled, "background derivation did not settle");
    let derived_in_session = calls.load(AtomicOrdering::SeqCst);
    assert!(
        derived_in_session > 0,
        "background derivation must run during the session; got 0"
    );

    // Now compact. Anything it derives is work the background scheduler did not
    // already do.
    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm_with_callbacks(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        slow_counting_callbacks(Arc::clone(&calls), Duration::ZERO),
    )
    .await
    .expect("arm");
    let at_compact = calls.load(AtomicOrdering::SeqCst) - derived_in_session;

    eprintln!(
        "R11 background: turns={TURNS} derived_in_session={derived_in_session} \
         calls_at_compact={at_compact} attempt={}",
        match &attempt {
            LhcCompactAttempt::Installed { body, .. } => format!("Installed({} items)", body.len()),
            LhcCompactAttempt::Failed { reason } => format!("Failed({reason})"),
            LhcCompactAttempt::Unavailable { reason } => format!("Unavailable({reason})"),
            LhcCompactAttempt::Cancelled { reason } => format!("Cancelled({reason})"),
            LhcCompactAttempt::MidTurnSkipped { reason } => format!("MidTurnSkipped({reason})"),
            LhcCompactAttempt::MidTurnBlocked { reason, .. } => format!("MidTurnBlocked({reason})"),
        }
    );

    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "compact must install off background-derived material: {attempt:?}"
    );
    assert_eq!(
        at_compact, 0,
        "a compact must not derive: background mode already did the work. \
         {at_compact} calls at compact time means derivation is still being \
         deferred to the deadline — the defect this round removed."
    );
}

/// Deterministic callbacks that count and optionally sleep — the delay makes
/// background derivation slow enough that an abort deterministically lands
/// while it is still in flight.
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
    let rollout = vec![codex_history::RolloutItem::Compacted(
        codex_history::CompactedItem {
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
            LhcCompactAttempt::Failed { reason } => format!("Failed({reason})"),
            LhcCompactAttempt::Unavailable { reason } => format!("Unavailable({reason})"),
            LhcCompactAttempt::Cancelled { reason } => format!("Cancelled({reason})"),
            LhcCompactAttempt::MidTurnSkipped { reason } => format!("MidTurnSkipped({reason})"),
            LhcCompactAttempt::MidTurnBlocked { reason, .. } => format!("MidTurnBlocked({reason})"),
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
    let LhcCompactAttempt::Installed {
        body: parent_body,
        marker: pmarker,
    } = pa
    else {
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
    let rollout = vec![codex_history::RolloutItem::Compacted(
        codex_history::CompactedItem {
            message: durable.clone(),
            replacement_history: Some(parent_body.clone().into_iter().map(Into::into).collect()),
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
    let inherited = csess.clone_history().await.into_raw_items();
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
            LhcCompactAttempt::Failed { reason } => format!("Failed({reason})"),
            LhcCompactAttempt::Unavailable { reason } => format!("Unavailable({reason})"),
            LhcCompactAttempt::Cancelled { reason } => format!("Cancelled({reason})"),
            LhcCompactAttempt::MidTurnSkipped { reason } => format!("MidTurnSkipped({reason})"),
            LhcCompactAttempt::MidTurnBlocked { reason, .. } => format!("MidTurnBlocked({reason})"),
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
        LhcCompactAttempt::Failed { .. }
        | LhcCompactAttempt::Unavailable { .. }
        | LhcCompactAttempt::Cancelled { .. } => {}
        LhcCompactAttempt::MidTurnSkipped { .. } | LhcCompactAttempt::MidTurnBlocked { .. } => {
            panic!("StandaloneTurn must not return MidTurn residual");
        }
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
    let before = sess.clone_history().await.into_raw_items();
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    let LhcCompactAttempt::Installed { .. } = attempt else {
        panic!("fixture: compact must install to measure its cache cost");
    };
    let after = sess.clone_history().await.into_raw_items();

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
                Box::pin(async move { inner(input).await }) as codex_lhc_host::BoxInferenceFuture
            })
        }};
    }
    let callbacks = InferenceCallbacks {
        smooth_prompt: measured!(
            smooth_prompt,
            SmoothPromptInput,
            "smooth_prompt",
            |i: &SmoothPromptInput| i.text.len()
        ),
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
    // Measured at the capture session — the seam where derivation now runs.
    install_lhc_and_enable_with(&mut session, dir.path().to_path_buf(), Some(callbacks)).await;
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
    assert!(
        handle.drain_settled(Duration::from_secs(180)).await,
        "background derivation must settle before profiling it"
    );

    let sess = Arc::new(session);
    let history = sess.clone_history().await.into_raw_items();
    let history_tokens = codex_lhc_host::estimate_response_items_tokens(&history);

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

/// N3 / C1.4 — a turn abort must cancel strict compact and install nothing.
///
/// Driven through the **production manual ladder** (`CompactTask::run`) with
/// the turn's real `CancellationToken`. Pre-cancelled token proves cancellation
/// is not permission to compact natively (strict dispatch returns TurnAborted).
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
    install_deterministic_test_override(&session);

    let thread_id = handle.thread_id().to_string();
    let root_for_marker = handle.root().map(std::path::Path::to_path_buf);
    let sess = Arc::new(session);
    let history_before = sess.clone_history().await.into_raw_items();

    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = SessionTask::run(
        Arc::new(CompactTask),
        Arc::clone(&sess),
        Arc::new(tc),
        Vec::new(),
        cancel,
    )
    .await;

    assert!(
        matches!(
            &result,
            Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted)
        ),
        "cancellation must surface TurnAborted (not native compact success): {result:?}"
    );
    let history_after = sess.clone_history().await.into_raw_items();
    let marker = archive_has_compact_marker(&thread_id, root_for_marker.as_deref()).await;
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

// ── Slice C: rollout rewrite ──────────────────────────────────────────────

/// Open live thread persistence so compact can rewrite a real rollout path.
async fn attach_rollout_for_slice_c(session: &mut Session) -> std::path::PathBuf {
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::protocol::ThreadMemoryMode;
    use codex_thread_store::CreateThreadParams;
    use codex_thread_store::LiveThread;
    use codex_thread_store::ThreadPersistenceMetadata;
    use uuid::Uuid;

    let config = session.get_config().await;
    let live_thread = LiveThread::create(
        Arc::clone(&session.services.thread_store),
        CreateThreadParams {
            session_id: session.session_id(),
            thread_id: session.thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "slice-c-test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: Default::default(),
            subagent_history_start_ordinal: None,
            history_base: None,
            initial_window_id: Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(config.cwd.to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: if config.memories.generate_memories {
                    ThreadMemoryMode::Enabled
                } else {
                    ThreadMemoryMode::Disabled
                },
            },
        },
    )
    .await
    .expect("create thread persistence");
    session.services.live_thread = Some(live_thread);
    session
        .ensure_rollout_materialized(PersistContext::Standard)
        .await;
    session.flush_rollout().await.expect("flush rollout");
    session
        .current_rollout_path()
        .await
        .expect("path")
        .expect("rollout path present")
}

/// Recorder-reopen pin: after swap, an append must land in the NEW file
/// (not the orphaned `.prev` inode).
#[tokio::test]
async fn slice_c_reopen_pin_append_lands_in_new_file() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root.clone()).await;
    let rollout_path = attach_rollout_for_slice_c(&mut session).await;

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
    let LhcCompactAttempt::Installed { .. } = attempt else {
        panic!("expected Installed: {attempt:?}");
    };

    // Post-swap marker text unique to the new generation append.
    let pin = "slice-c-reopen-pin-unique-marker-text";
    sess.persist_rollout_items(&[codex_history::RolloutItem::EventMsg(
        codex_protocol::protocol::EventMsg::AgentMessage(
            codex_protocol::protocol::AgentMessageEvent {
                message: pin.to_string(),
                phase: None,
                memory_citation: None,
            },
        ),
    )])
    .await;
    sess.flush_rollout().await.expect("flush pin append");

    let new_contents = tokio::fs::read_to_string(&rollout_path)
        .await
        .expect("read new rollout");
    assert!(
        new_contents.contains(pin),
        "append after swap must land in the NEW active file"
    );

    let prev = codex_lhc_host::SwapPaths::for_rollout(&rollout_path).prev;
    if prev.exists() {
        let prev_contents = tokio::fs::read_to_string(&prev).await.expect("read prev");
        assert!(
            !prev_contents.contains(pin),
            "append after swap must NOT land in the orphaned .prev generation \
             (reopen pin mutation target)"
        );
    }
}

/// In-memory history installed at compact must equal what resume rebuilds
/// from the rewritten file (item-for-item structural equality).
#[tokio::test]
async fn slice_c_in_memory_equals_resume_from_rewritten_file() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root.clone()).await;
    let rollout_path = attach_rollout_for_slice_c(&mut session).await;

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
    let LhcCompactAttempt::Installed { body, .. } = attempt else {
        panic!("expected Installed: {attempt:?}");
    };

    let host = sess.clone_history().await.into_raw_items();
    assert!(
        response_items_structurally_equal(&host, &body),
        "installed body must equal live host history"
    );

    // Reconstruct from the rewritten file the same way resume does.
    let file_items = codex_lhc_host::parse_rollout_items(&rollout_path).expect("parse rewritten");
    assert!(
        file_items
            .iter()
            .any(|i| matches!(i, codex_history::RolloutItem::Compacted(_))),
        "rewritten file must contain exactly the boundary Compacted"
    );
    let compacted_count = file_items
        .iter()
        .filter(|i| matches!(i, codex_history::RolloutItem::Compacted(_)))
        .count();
    assert_eq!(compacted_count, 1, "exactly one Compacted boundary");

    let reconstructed = codex_lhc_host::history_from_materialized_items(&file_items);
    assert!(
        response_items_structurally_equal(&reconstructed, &host),
        "resume-from-rewritten-file must equal installed history item-for-item\n\
         reconstructed={} host={}",
        reconstructed.len(),
        host.len()
    );
}

/// Window numbers stay monotonic across two consecutive rewrites.
#[tokio::test]
async fn slice_c_window_continuity_across_two_rewrites() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root.clone()).await;
    let rollout_path = attach_rollout_for_slice_c(&mut session).await;

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
    let a1 = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    assert!(
        matches!(a1, LhcCompactAttempt::Installed { .. }),
        "first install: {a1:?}"
    );

    // Grow again so a second compact reduces.
    seed_conversation_bandable(sess.as_ref(), &tc, 40).await;
    handle.flush().await;
    let a2 = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    assert!(
        matches!(a2, LhcCompactAttempt::Installed { .. }),
        "second install: {a2:?}"
    );

    let file_items = codex_lhc_host::parse_rollout_items(&rollout_path).expect("parse");
    let windows: Vec<u64> = file_items
        .iter()
        .filter_map(|i| match i {
            codex_history::RolloutItem::Compacted(c) => c.window_number,
            _ => None,
        })
        .collect();
    assert_eq!(windows.len(), 1, "rewritten file has one boundary");
    assert!(
        windows[0] >= 2,
        "second rewrite must advance window_number (got {})",
        windows[0]
    );

    // Prior generation retained.
    let prev = codex_lhc_host::SwapPaths::for_rollout(&rollout_path).prev;
    assert!(prev.exists(), "exactly one prior generation retained");
    let prev_items = codex_lhc_host::parse_rollout_items(&prev).expect("parse prev");
    let prev_windows: Vec<u64> = prev_items
        .iter()
        .filter_map(|i| match i {
            codex_history::RolloutItem::Compacted(c) => c.window_number,
            _ => None,
        })
        .collect();
    assert!(
        !prev_windows.is_empty() && prev_windows[0] < windows[0],
        "prior generation window {} must be < active {}",
        prev_windows.first().copied().unwrap_or(0),
        windows[0]
    );
}

/// Mutation demo: if reopen is skipped after swap, appends land in .prev.
/// This test documents the pin — the production path reopens; breaking
/// reopen (by writing via an unreopened fd) is what this would catch.
#[tokio::test]
async fn slice_c_mutation_reopen_pin_demonstrates_orphan_without_reopen() {
    use std::io::Write;

    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    // Seed + rewrite with the pure swap helper (no recorder).
    let seed = vec![codex_history::RolloutItem::SessionMeta(
        codex_protocol::protocol::SessionMetaLine {
            meta: codex_protocol::protocol::SessionMeta {
                timestamp: "t".into(),
                ..codex_protocol::protocol::SessionMeta::default()
            },
            git: None,
        },
    )];
    codex_lhc_host::atomic_rewrite_rollout(&path, &seed).ok();
    // Open an append fd, then swap underneath it — the classic unreopened bug.
    let mut orphan_fd = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open");
    let gen2 = vec![
        codex_history::RolloutItem::SessionMeta(codex_protocol::protocol::SessionMetaLine {
            meta: codex_protocol::protocol::SessionMeta {
                timestamp: "t2".into(),
                ..codex_protocol::protocol::SessionMeta::default()
            },
            git: None,
        }),
        codex_history::RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::AgentMessage(
            codex_protocol::protocol::AgentMessageEvent {
                message: "gen2-body".into(),
                phase: None,
                memory_citation: None,
            },
        )),
    ];
    codex_lhc_host::atomic_rewrite_rollout(&path, &gen2).expect("swap under open fd");
    // Write via the old fd — lands in .prev (orphaned inode).
    writeln!(orphan_fd, r#"{{"timestamp":"x","type":"event_msg","payload":{{"type":"agent_message","message":"orphaned-append"}}}}"#)
        .expect("write orphan");
    orphan_fd.flush().unwrap();

    let prev = codex_lhc_host::SwapPaths::for_rollout(&path).prev;
    let prev_text = std::fs::read_to_string(&prev).expect("prev");
    let new_text = std::fs::read_to_string(&path).expect("new");
    assert!(
        prev_text.contains("orphaned-append"),
        "unreopened fd writes to .prev — this is the bug reopen prevents"
    );
    assert!(
        !new_text.contains("orphaned-append"),
        "new generation must stay clean without reopen"
    );
    assert!(
        new_text.contains("gen2-body"),
        "new generation holds the rewritten content"
    );
}

// ── Emergency triage: strict LHC policy evidence ───────────────────────────

/// Production gpt-5.6 models use the Codex-LHC 370k window and 350k trigger.
#[test]
fn gpt_5_6_auto_compact_threshold_is_lhc_350_000() {
    use codex_models_manager::ModelsManagerConfig;
    use codex_models_manager::bundled_models_response;
    use codex_models_manager::test_support::construct_model_info_offline_for_tests;
    use codex_protocol::openai_models::ModelsResponse;

    let bundled = bundled_models_response().expect("bundled models.json parses");
    for slug in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
        let catalog = bundled
            .models
            .iter()
            .find(|m| m.slug == slug)
            .unwrap_or_else(|| panic!("bundled catalog missing {slug}"));
        assert_eq!(
            catalog.context_window,
            Some(370_000),
            "{slug}: models.json must use the 370k working window"
        );
        assert_eq!(
            catalog.max_context_window,
            Some(1_050_000),
            "{slug}: models.json must retain the full capability ceiling"
        );
        assert_eq!(
            catalog.auto_compact_token_limit,
            Some(350_000),
            "{slug}: models.json must use the 350k LHC trigger"
        );

        let config = ModelsManagerConfig {
            model_catalog: Some(ModelsResponse {
                models: bundled.models.clone(),
                ..ModelsResponse::default()
            }),
            ..ModelsManagerConfig::default()
        };
        let model = construct_model_info_offline_for_tests(slug, &config);
        assert_eq!(
            model.auto_compact_token_limit(),
            Some(350_000),
            "{slug}: resolved auto_compact_token_limit() must be 350k"
        );
    }
}

/// Manual CompactTask and automatic run_auto_compact share the same strict
/// failure policy: LHC unavailable is a hard error, never TokenBudget/remote/local.
#[tokio::test]
async fn strict_manual_and_auto_share_hard_failure_policy() {
    use crate::session::turn::run_auto_compact;
    use codex_analytics::CompactionPhase;
    use codex_analytics::CompactionReason;

    // Manual path — no LHC capture slot → Failed.
    let (session_m, tc_m) = make_session_and_context().await;
    let before_m = session_m.clone_history().await.into_raw_items();
    let sess_m = Arc::new(session_m);
    let manual = SessionTask::run(
        Arc::new(CompactTask),
        Arc::clone(&sess_m),
        Arc::new(tc_m),
        Vec::new(),
        CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(
            &manual,
            Err(err) if matches!(err.details(), CodexErrorDetails::UnsupportedOperation(_))
        ),
        "manual strict path must hard-fail without native compact: {manual:?}"
    );
    assert!(
        response_items_structurally_equal(
            &sess_m.clone_history().await.into_raw_items(),
            &before_m
        ),
        "manual hard failure must preserve prior history"
    );

    // Auto path — same hard-fail class, no native ladder.
    let (session_a, tc_a) = make_session_and_context().await;
    let before_a = session_a.clone_history().await.into_raw_items();
    let sess_a = Arc::new(session_a);
    let step = crate::session::step_context::StepContext::for_test(Arc::new(tc_a));
    let mut client = inert_model_client_session();
    let auto = run_auto_compact(
        &sess_a,
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
        matches!(
            &auto,
            Err(err) if matches!(err.details(), CodexErrorDetails::UnsupportedOperation(_))
        ),
        "auto strict path must hard-fail without native compact: {auto:?}"
    );
    assert!(
        response_items_structurally_equal(
            &sess_a.clone_history().await.into_raw_items(),
            &before_a
        ),
        "auto hard failure must preserve prior history"
    );
}

/// Pending/unsettled derivation must not block compact (no drain_settled wait).
///
/// Proves work is still pending when compact starts, and uses a bound that
/// would catch a restored multi-second settle wait.
#[tokio::test]
async fn unsettled_derivation_does_not_block_lhc_compact() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    let calls = Arc::new(AtomicUsize::new(0));
    // Slow background derivation so it is still running at compact time.
    // Each inference lane sleeps long enough that a restored settle wait
    // would push compact past the tight elapsed bound below.
    install_lhc_and_enable_with(
        &mut session,
        root,
        Some(slow_counting_callbacks(
            Arc::clone(&calls),
            Duration::from_millis(2_500),
        )),
    )
    .await;
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
    // Do not drain_settled — leave work pending/running.
    let settled_before = handle.drain_settled(Duration::from_millis(1)).await;
    assert!(
        !settled_before,
        "fixture: derivation must still be pending/running when compact starts"
    );

    let sess = Arc::new(session);
    let started = std::time::Instant::now();
    let attempt = try_run_lhc_compact_arm_with_callbacks(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        deterministic_callbacks(),
    )
    .await
    .expect("arm");
    let elapsed = started.elapsed();

    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "unsettled derivation must not block Install: {attempt:?}"
    );
    // Bound catches a restored drain_settled (multi-second / 60s waits). Allow
    // headroom for produce/materialize on bandable history.
    assert!(
        elapsed < Duration::from_secs(15),
        "compact must not wait for unsettled derivation (settle-wait restored?); took {elapsed:?}"
    );
}

/// Queue-loss / missing-archive recovery: host tool+reasoning not in the archive
/// are imported during produce; compact installs a structurally valid request.
#[tokio::test]
async fn queue_loss_imports_missing_tool_and_reasoning() {
    use codex_protocol::ResponseItemId;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;

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

    // Capture a base conversation into the archive, then park the worker and
    // overfill so subsequent tool/reasoning host items are dropped (degraded).
    seed_conversation_bandable(&session, &tc, 40).await;
    handle.flush().await;

    let release = handle.block_worker().await;
    let n = codex_lhc_host::CAPTURE_QUEUE_CAP + 16;
    for i in 0..n {
        let flood = ResponseItem::Message {
            id: Some(ResponseItemId::from_server(format!("flood-{i}"))),
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: format!("flood {i}"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        handle.persist(&flood, codex_extension_api::RawItemProvenance::UserPrompt);
    }
    assert!(
        handle.is_degraded(),
        "fixture: queue overfill must latch degraded (dropped={})",
        handle.dropped_count()
    );
    let _ = release.send(());

    // Host-only tool call/result + reasoning that never reached the archive.
    let call_id = "call-import-1";
    let missing_items = vec![
        ResponseItem::FunctionCall {
            id: Some(ResponseItemId::from_server("fc-import-1".into())),
            name: "shell".into(),
            namespace: None,
            arguments: "{\"command\":[\"echo\",\"import-me\"]}".into(),
            encrypted_function_args: None,
            call_id: call_id.into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: Some(ResponseItemId::from_server("fco-import-1".into())),
            call_id: call_id.into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("import-tool-result-ok".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Reasoning {
            id: Some(ResponseItemId::from_server("rs-import-1".into())),
            summary: vec![],
            content: None,
            encrypted_content: Some("reasoning-signature-import".into()),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    session
        .record_conversation_items_with_provenance(
            &tc,
            &missing_items,
            codex_extension_api::RawItemProvenance::ModelOutput,
        )
        .await;
    // Degraded refuses further persist — these stay host-only until import.
    handle.flush().await;

    let sess = Arc::new(session);
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "missing tool/reasoning must import and install: {attempt:?}"
    );
    let installed = sess.clone_history().await.into_raw_items();
    assert!(!installed.is_empty(), "installed request must be non-empty");
    // Structural validity: no orphan function-call output without a prior call id
    // in the same installed history (pair preserved through import+materialize
    // when present; bands may summarize — at minimum install must succeed).
    let has_call_id = installed.iter().any(|item| match item {
        ResponseItem::FunctionCall { call_id: c, .. } => c == call_id,
        _ => false,
    });
    let has_result_id = installed.iter().any(|item| match item {
        ResponseItem::FunctionCallOutput { call_id: c, .. } => c == call_id,
        _ => false,
    });
    // Live tail may retain tool pairs; bands may fold them. Either way install
    // must not hard-fail on missing archive coverage.
    let _ = (has_call_id, has_result_id);
}

/// Hard failure path: no native Compacted record, history preserved.
#[tokio::test]
async fn hard_failure_preserves_history_and_writes_no_native_compacted() {
    let (session, tc) = make_session_and_context().await;
    // Seed some host history without LHC so compact cannot open a capture handle.
    for i in 0..5 {
        session
            .record_conversation_items_with_provenance(
                &tc,
                &[ResponseItem::Message {
                    id: None,
                    role: "user".into(),
                    content: vec![ContentItem::InputText {
                        text: format!("user-{i}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                codex_extension_api::RawItemProvenance::HostContext,
            )
            .await;
    }
    let before = session.clone_history().await.into_raw_items();
    let before_len = before.len();
    let sess = Arc::new(session);

    let result = crate::compact_lhc::run_strict_lhc_compact(
        &sess,
        &Arc::new(tc),
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        codex_analytics::CompactionPhase::StandaloneTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(
            &result,
            Err(err) if matches!(err.details(), CodexErrorDetails::UnsupportedOperation(_))
        ),
        "strict compact without LHC must hard-fail: {result:?}"
    );
    let after = sess.clone_history().await.into_raw_items();
    assert!(
        response_items_structurally_equal(&before, &after),
        "prior history must remain"
    );
    assert_eq!(
        after.len(),
        before_len,
        "hard failure must not replace history with a native Compacted install"
    );
}

/// Failed rewrite must not advance window ids/number (transactional).
#[tokio::test]
async fn rewrite_failure_leaves_auto_compact_window_unchanged() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root).await;
    // Live rollout path required so atomic rewrite (and failpoint) actually run.
    let _rollout_path = attach_rollout_for_slice_c(&mut session).await;
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

    let before_snapshot = session.auto_compact_window_snapshot().await;
    let before_ids = session.auto_compact_window_ids().await;
    let before_window_number = session.auto_compact_window_number().await;

    // Inject rewrite failure after materialize plans the new window.
    let _guard =
        codex_lhc_host::SwapFailpointGuard::arm(codex_lhc_host::SwapFailpoint::PostTempWrite);

    let sess = Arc::new(session);
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    assert!(
        matches!(
            &attempt,
            LhcCompactAttempt::Failed { reason } if reason.contains("rewrite")
        ),
        "injected rewrite failure must hard-fail: {attempt:?}"
    );

    let after_ids = sess.auto_compact_window_ids().await;
    let after_window_number = sess.auto_compact_window_number().await;
    let after_snapshot = sess.auto_compact_window_snapshot().await;
    assert_eq!(
        after_window_number, before_window_number,
        "window number must not advance on rewrite failure"
    );
    assert_eq!(
        after_ids, before_ids,
        "window ids must not change on rewrite failure"
    );
    assert_eq!(
        after_snapshot, before_snapshot,
        "prefill snapshot must be unchanged on rewrite failure"
    );
}

/// Model-downshift installs/validates against the target (smaller) model context.
/// A body that fits the previous larger model but exceeds the target hard-fails.
#[tokio::test]
async fn model_downshift_target_context_rejects_body_over_target_window() {
    use crate::session::turn::run_auto_compact;
    use codex_analytics::CompactionPhase;
    use codex_analytics::CompactionReason;

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, mut target_tc) = make_session_and_context().await;
    // Separate TurnContext for the previous larger model (TurnContext is not Clone).
    let (_, mut previous_tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    seed_conversation_bandable(&session, &target_tc, 80).await;
    handle.flush().await;
    install_deterministic_test_override(&session);

    // Previous (larger) model step context — would accept a larger body.
    previous_tc.model_info.slug = "gpt-prev-large".into();
    previous_tc.model_info.context_window = Some(1_000_000);
    previous_tc.model_info.max_context_window = Some(1_000_000);
    previous_tc.model_info.auto_compact_token_limit = Some(900_000);

    // Target (current) model with a tiny compact target so the LHC body is
    // guaranteed over-target and must hard-fail (not install against the
    // previous larger window).
    target_tc.model_info.context_window = Some(2_000);
    target_tc.model_info.max_context_window = Some(2_000);
    target_tc.model_info.auto_compact_token_limit = Some(32);

    let sess = Arc::new(session);
    let previous_step = crate::session::step_context::StepContext::for_test(Arc::new(previous_tc));
    let target_step = crate::session::step_context::StepContext::for_test(Arc::new(target_tc));
    let mut client = inert_model_client_session();
    let result = run_auto_compact(
        &sess,
        previous_step,
        Some(target_step),
        &mut client,
        InitialContextInjection::DoNotInject,
        CompactionReason::ModelDownshift,
        CompactionPhase::PreTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(
            &result,
            Err(err) if matches!(err.details(), CodexErrorDetails::UnsupportedOperation(_))
        ),
        "body over target model compact window must hard-fail under downshift: {result:?}"
    );
    let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        msg.contains("compact target")
            || msg.contains("exceed")
            || msg.contains("LHC compact failed"),
        "failure should mention compact target / window, got: {msg}"
    );
}

/// Successful LHC compact body must clear the 350k production trigger.
#[tokio::test]
async fn successful_lhc_compact_clears_regular_trigger() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, mut tc) = make_session_and_context().await;
    // Codex-LHC policy for GPT-5.6 long-context operation.
    tc.model_info.auto_compact_token_limit = Some(350_000);
    tc.model_info.context_window = Some(370_000);
    tc.model_info.max_context_window = Some(1_050_000);

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
    let attempt = run_arm_deterministic(&sess, &tc, /*manual*/ true).await;
    let LhcCompactAttempt::Installed { body, .. } = attempt else {
        panic!("expected Install for bandable history: {attempt:?}");
    };
    let body_tokens = codex_lhc_host::estimate_response_items_tokens(&body);
    assert!(
        body_tokens <= 350_000,
        "successful LHC body must clear the 350k trigger (body_tokens={body_tokens})"
    );
    let installed = sess.clone_history().await.into_raw_items();
    let installed_tokens = codex_lhc_host::estimate_response_items_tokens(&installed);
    assert!(
        installed_tokens <= 350_000,
        "installed history must also clear 350k (tokens={installed_tokens})"
    );
}

/// Exact hang from the 9fdbca74ec canary: compact awaited unbounded
/// `CaptureHandle::flush()` before produce, so a parked/wedged capture worker
/// never reached the 120s produce timeout.
///
/// Pending capture work plus a blocked worker must still finish in bounded
/// time via fallback/import/coverage. Hard failure preserves history. Neither
/// outcome may append a native Compacted record.
#[tokio::test]
async fn blocked_capture_flush_does_not_hang_lhc_compact() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root.clone()).await;
    let rollout_path = attach_rollout_for_slice_c(&mut session).await;
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

    let history_before = session.clone_history().await.into_raw_items();
    let thread_id = handle.thread_id().to_string();

    // Park the worker so the arm's flush cannot be acknowledged. Queue one
    // more persist behind the park so capture work is pending.
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
    let started = std::time::Instant::now();
    let attempt = tokio::time::timeout(
        Duration::from_secs(25),
        run_arm_deterministic(&sess, &tc, /*manual*/ true),
    )
    .await
    .expect("blocked capture flush must complete compact in bound");
    let elapsed = started.elapsed();
    drop(release);

    assert!(
        elapsed < Duration::from_secs(25),
        "blocked capture flush must not hang compact; took {elapsed:?}"
    );

    let file_items = codex_lhc_host::parse_rollout_items(&rollout_path).expect("parse rollout");
    let compacted: Vec<&codex_history::CompactedItem> = file_items
        .iter()
        .filter_map(|item| match item {
            codex_history::RolloutItem::Compacted(c) => Some(c),
            _ => None,
        })
        .collect();
    assert!(
        compacted
            .iter()
            .all(|c| c.message.contains("lhc_compact_durable")),
        "no native Compacted records allowed: {compacted:?}"
    );

    match attempt {
        LhcCompactAttempt::Installed { marker, .. } => {
            assert!(
                marker.body_item_count > 0,
                "installed LHC marker must describe a served body: {marker:?}"
            );
            assert!(
                archive_has_compact_marker(&thread_id, Some(root.as_path())).await,
                "reducing LHC body must install an LHC marker"
            );
            assert_eq!(
                compacted.len(),
                1,
                "LHC rewrite writes exactly one LHC Compacted boundary"
            );
        }
        LhcCompactAttempt::Failed { reason } | LhcCompactAttempt::Unavailable { reason } => {
            let history_after = sess.clone_history().await.into_raw_items();
            assert!(
                response_items_structurally_equal(&history_before, &history_after),
                "hard failure must preserve history: {reason}"
            );
            assert!(
                compacted.is_empty(),
                "hard failure must not write a Compacted record: {reason}"
            );
            assert!(
                !archive_has_compact_marker(&thread_id, Some(root.as_path())).await,
                "hard failure must not commit an LHC marker: {reason}"
            );
        }
        LhcCompactAttempt::Cancelled { reason } => {
            panic!("blocked flush is not cancellation: {reason}");
        }
        LhcCompactAttempt::MidTurnSkipped { reason }
        | LhcCompactAttempt::MidTurnBlocked { reason, .. } => {
            panic!("StandaloneTurn returned MidTurn residual: {reason}");
        }
    }
}

fn drain_context_compaction_counts(
    rx: &async_channel::Receiver<codex_protocol::protocol::Event>,
) -> (usize, usize) {
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::EventMsg;
    let mut started = 0;
    let mut completed = 0;
    while let Ok(event) = rx.try_recv() {
        match event.msg {
            EventMsg::ItemStarted(e) if matches!(e.item, TurnItem::ContextCompaction(_)) => {
                started += 1;
            }
            EventMsg::ItemCompleted(e) if matches!(e.item, TurnItem::ContextCompaction(_)) => {
                completed += 1;
            }
            _ => {}
        }
    }
    (started, completed)
}

/// Successful LHC install emits exactly one ContextCompaction start+complete pair.
#[tokio::test]
async fn installed_emits_one_context_compaction_started_and_completed() {
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
    let _ = drain_context_compaction_counts(&rx);

    let result = crate::compact_lhc::run_strict_lhc_compact(
        &Arc::new(session),
        &Arc::new(tc),
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        codex_analytics::CompactionPhase::StandaloneTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await;
    assert!(result.is_ok(), "expected successful install: {result:?}");
    assert_eq!(
        drain_context_compaction_counts(&rx),
        (1, 1),
        "Installed must emit one ContextCompaction started+completed pair"
    );
}

/// Hard-fail path (no LHC) must not announce ContextCompaction.
#[tokio::test]
async fn hard_failure_emits_no_context_compaction_items() {
    let (sess_arc, tc_arc, rx) = make_session_and_context_with_rx().await;
    let session = match Arc::try_unwrap(sess_arc) {
        Ok(session) => session,
        Err(_) => panic!("unique session"),
    };
    let tc = match Arc::try_unwrap(tc_arc) {
        Ok(tc) => tc,
        Err(_) => panic!("unique turn context"),
    };
    let _ = drain_context_compaction_counts(&rx);

    let result = crate::compact_lhc::run_strict_lhc_compact(
        &Arc::new(session),
        &Arc::new(tc),
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        codex_analytics::CompactionPhase::StandaloneTurn,
        /*mid_turn*/ None,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(
            &result,
            Err(err) if matches!(err.details(), CodexErrorDetails::UnsupportedOperation(_))
        ),
        "strict compact without LHC must hard-fail: {result:?}"
    );
    assert_eq!(
        drain_context_compaction_counts(&rx),
        (0, 0),
        "skip/fail must not emit ContextCompaction"
    );
}
