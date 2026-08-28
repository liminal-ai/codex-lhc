//! Required-compact readiness proofs (LIM-134).
//!
//! A PreTurn / Standalone strict compact that is already known to be required
//! waits, bounded and cancellably, for the asynchronous capture open. Ready
//! continues; `Failed`, `Stopped`, and bound expiry are hard visible failures
//! that preserve the prior body and issue zero provider requests; cancellation
//! aborts the turn. MidTurn keeps its own settled in-flight policy.
//!
//! The slot is held `Opening` by `install_with_root_held_open`, and the wait is
//! shown to actually block by advancing the **paused tokio clock** — virtual,
//! exact, and free of wall-clock cost. Timeouts appear only as deadlock
//! ceilings. There is no process-global bound override, so parallel tests
//! cannot observe each other.

use std::sync::Arc;
use std::time::Duration;

use codex_analytics::CompactionPhase;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadStartInput;
use codex_features::Feature;
use codex_lhc_host::CaptureState;
use codex_lhc_host::InferenceCallbacks;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::install_with_root_held_open;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use codex_thread_store::PersistContext;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::LhcCompactAttempt;
use super::STRICT_COMPACT_READINESS_BOUND;
use super::try_run_lhc_compact_arm;
use super::try_run_lhc_compact_arm_with_callbacks_and_cancel;
use crate::compact::InitialContextInjection;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;

/// Deadlock ceiling only — never the evidence that ordering happened.
const DEADLOCK_CEILING: Duration = Duration::from_secs(60);

fn deterministic_callbacks() -> InferenceCallbacks {
    codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic offline callbacks")
}

fn text_input(text: &str) -> UserInput {
    UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }
}

/// Install LHC with the capture slot deliberately held in `Opening`.
async fn install_held_open(session: &mut Session, root: std::path::PathBuf) -> Arc<LhcCaptureSlot> {
    session
        .set_feature_for_test(Feature::LhcCapture, true)
        .expect("enable LhcCapture");
    let mut builder = ExtensionRegistryBuilder::<crate::config::Config>::new();
    install_with_root_held_open(&mut builder, |_c| true, root);
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
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("held-open slot");
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "the held-open installer must leave the slot Opening"
    );
    slot
}

async fn seed_bandable(session: &Session, tc: &TurnContext, turns: usize) {
    let pad = "p".repeat(2500);
    for i in 0..turns {
        session
            .record_user_prompt_and_emit_turn_item(
                tc,
                &[text_input(&format!("user turn {i} readiness seed {pad}"))],
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
                        text: format!("assistant reply {i} readiness seed {pad}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                codex_extension_api::RawItemProvenance::ModelOutput,
            )
            .await;
    }
}

async fn open_real_handle(
    root: &std::path::Path,
    thread_id: &str,
) -> codex_lhc_host::CaptureHandle {
    let derivation = codex_lhc_host::LateBoundCallbacks::new();
    derivation.seed(deterministic_callbacks());
    codex_lhc_host::spawn_capture(thread_id, None, Some(root.to_path_buf()), derivation)
        .await
        .expect("open capture")
}

/// Spawn the required strict-compact arm against a held-open slot.
fn spawn_required_compact(
    sess: Arc<Session>,
    tc: Arc<TurnContext>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<LhcCompactAttempt> {
    tokio::spawn(async move {
        try_run_lhc_compact_arm_with_callbacks_and_cancel(
            &sess,
            tc.as_ref(),
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            deterministic_callbacks(),
            &cancel,
        )
        .await
        .expect("arm")
    })
}

/// The production safety bound is exactly 60 seconds. A drift here changes how
/// long a required compact can stall a turn, so it is pinned explicitly.
#[test]
fn production_readiness_bound_is_sixty_seconds() {
    assert_eq!(STRICT_COMPACT_READINESS_BOUND, Duration::from_secs(60));
}

/// Held open: the required compact does not resolve while the slot is
/// `Opening` — proved by advancing the virtual clock well inside the bound —
/// and completes through the strict path only after Ready.
#[tokio::test]
async fn required_compact_waits_for_a_held_open_capture_then_proceeds() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let (mut session, tc) = make_session_and_context().await;
    let slot = install_held_open(&mut session, root.clone()).await;
    seed_bandable(&session, &tc, 40).await;
    let sess = Arc::new(session);
    let tc = Arc::new(tc);

    let arm = spawn_required_compact(Arc::clone(&sess), Arc::clone(&tc), CancellationToken::new());

    tokio::time::pause();
    tokio::time::advance(STRICT_COMPACT_READINESS_BOUND / 2).await;
    assert!(
        !arm.is_finished(),
        "required compact must wait for the capture open, not declare it not-ready"
    );
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "the slot is still Opening — the arm has nothing to proceed on"
    );
    // Real time again: the produce path below runs real workers and timers.
    tokio::time::resume();

    let thread_id = sess.thread_id.to_string();
    let handle = open_real_handle(&root, &thread_id).await;
    slot.set_derivation_callbacks(deterministic_callbacks());
    assert!(
        slot.publish_ready_for_test(handle),
        "Ready must publish onto a slot that is still Opening"
    );

    let attempt = arm.await.expect("arm join");
    assert!(
        matches!(attempt, LhcCompactAttempt::Installed { .. }),
        "the arm must proceed through the strict path once Ready lands: {attempt:?}"
    );
}

/// A permanently failed open is a hard visible compact failure — never
/// `Ok(None)`, never native fallback — and the prior body is preserved.
#[tokio::test]
async fn required_compact_hard_fails_when_the_open_failed() {
    let dir = tempdir().unwrap();
    let (mut session, tc) = make_session_and_context().await;
    let slot = install_held_open(&mut session, dir.path().join("lhc")).await;
    seed_bandable(&session, &tc, 4).await;
    let before = session.clone_history().await.into_raw_items().len();
    let sess = Arc::new(session);

    slot.publish_failed_for_test(codex_lhc_host::CAPTURE_OPEN_FAILED);
    let attempt = try_run_lhc_compact_arm_with_callbacks_and_cancel(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        deterministic_callbacks(),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");

    assert!(
        matches!(&attempt, LhcCompactAttempt::Failed { reason }
            if reason.contains(codex_lhc_host::CAPTURE_OPEN_FAILED)),
        "a failed open must surface as a hard compact failure: {attempt:?}"
    );
    assert_eq!(
        sess.clone_history().await.into_raw_items().len(),
        before,
        "a hard failure preserves the prior body"
    );
}

/// A stopped capture is likewise a hard visible failure, distinguishable from
/// "still opening".
#[tokio::test]
async fn required_compact_hard_fails_when_the_capture_stopped() {
    let dir = tempdir().unwrap();
    let (mut session, tc) = make_session_and_context().await;
    let slot = install_held_open(&mut session, dir.path().join("lhc")).await;
    seed_bandable(&session, &tc, 4).await;
    let sess = Arc::new(session);

    slot.publish_stopped_for_test();
    let attempt = try_run_lhc_compact_arm_with_callbacks_and_cancel(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        deterministic_callbacks(),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");

    assert!(
        matches!(&attempt, LhcCompactAttempt::Failed { reason } if reason.contains("shut down")),
        "a stopped capture must surface as a hard compact failure: {attempt:?}"
    );
}

/// Bound expiry is a hard visible failure too. The full production 60s bound is
/// exercised in virtual time, so the proof is exact and costs no wall clock.
#[tokio::test]
async fn required_compact_hard_fails_when_the_readiness_bound_expires() {
    let dir = tempdir().unwrap();
    let (mut session, tc) = make_session_and_context().await;
    let slot = install_held_open(&mut session, dir.path().join("lhc")).await;
    seed_bandable(&session, &tc, 4).await;
    let before = session.clone_history().await.into_raw_items().len();
    let sess = Arc::new(session);
    let tc = Arc::new(tc);

    let arm = spawn_required_compact(Arc::clone(&sess), Arc::clone(&tc), CancellationToken::new());

    tokio::time::pause();
    tokio::time::advance(STRICT_COMPACT_READINESS_BOUND / 2).await;
    assert!(!arm.is_finished(), "the arm must still be waiting");
    tokio::time::advance(STRICT_COMPACT_READINESS_BOUND).await;
    let attempt = arm.await.expect("arm join");
    tokio::time::resume();

    assert!(
        matches!(&attempt, LhcCompactAttempt::Failed { reason } if reason.contains("did not open")),
        "readiness-bound expiry must be a hard compact failure: {attempt:?}"
    );
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "expiry is the compact's decision; it does not mutate the slot"
    );
    assert_eq!(
        sess.clone_history().await.into_raw_items().len(),
        before,
        "expiry preserves the prior body"
    );
}

/// Turn cancellation during the readiness wait aborts the turn. It is not
/// permission for native compaction and not a silent success.
#[tokio::test]
async fn required_compact_aborts_the_turn_when_cancelled_while_waiting() {
    let dir = tempdir().unwrap();
    let (mut session, tc) = make_session_and_context().await;
    let _slot = install_held_open(&mut session, dir.path().join("lhc")).await;
    seed_bandable(&session, &tc, 4).await;
    let sess = Arc::new(session);
    let tc = Arc::new(tc);
    let cancel = CancellationToken::new();

    let arm = spawn_required_compact(Arc::clone(&sess), Arc::clone(&tc), cancel.clone());

    tokio::time::pause();
    tokio::time::advance(STRICT_COMPACT_READINESS_BOUND / 2).await;
    assert!(!arm.is_finished(), "the arm must still be waiting");
    cancel.cancel();
    let attempt = arm.await.expect("arm join");
    tokio::time::resume();

    assert!(
        matches!(&attempt, LhcCompactAttempt::Cancelled { reason } if reason.contains("cancelled")),
        "cancellation during the readiness wait must abort the turn: {attempt:?}"
    );
}

/// MidTurn must not inherit the readiness wait. It takes its own settled
/// in-flight policy — named in the returned reason — and returns immediately
/// rather than consuming the PreTurn bound.
#[tokio::test]
async fn mid_turn_does_not_inherit_the_readiness_wait() {
    let dir = tempdir().unwrap();
    let (mut session, tc) = make_session_and_context().await;
    let _slot = install_held_open(&mut session, dir.path().join("lhc")).await;
    *session
        .services
        .lhc_test_inference
        .lock()
        .expect("lhc_test_inference lock") = Some(deterministic_callbacks());
    let sess = Arc::new(session);

    let mid = super::MidTurnSeamFacts {
        attempt_id: "resp-midturn-readiness".into(),
        response_token_usage: None,
        response_tool_call_ids: Vec::new(),
        total_needs_follow_up: true,
        input_epoch_at_decision: 0,
        inside_transport_retry: false,
        model_response_complete: true,
    };
    let attempt = tokio::time::timeout(
        DEADLOCK_CEILING,
        try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid),
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("MidTurn must not park on the PreTurn readiness wait")
    .expect("arm");

    assert!(
        matches!(&attempt, LhcCompactAttempt::MidTurnBlocked { reason, .. }
            if reason.contains("not ready at MidTurn")),
        "MidTurn keeps its own settled in-flight policy: {attempt:?}"
    );
}
