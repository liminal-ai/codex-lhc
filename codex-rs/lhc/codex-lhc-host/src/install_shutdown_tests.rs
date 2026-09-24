//! Production lifecycle coverage for capture shutdown durability and bounds.

use super::*;

use crate::LateBoundCallbacks;
use crate::lhc_inference_callbacks;
use crate::spawn_capture;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::RawItemInput;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

struct LifecycleHarness {
    registry: ExtensionRegistry<()>,
    session_store: ExtensionData,
    thread_store: ExtensionData,
}

impl LifecycleHarness {
    async fn start(root: &std::path::Path, thread_id: &str) -> Self {
        let mut builder = ExtensionRegistryBuilder::<()>::new();
        install_with_root(&mut builder, |_config| true, root.to_path_buf());
        let registry = builder.build();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new(thread_id);
        let config = ();
        let session_source = SessionSource::Exec;
        let environments = [];

        for contributor in registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_start(ThreadStartInput {
                    config: &config,
                    session_source: &session_source,
                    persistent_thread_state_available: false,
                    environments: &environments,
                    mcp_resource_client: None,
                    extension_metrics: None,
                    session_store: &session_store,
                    thread_store: &thread_store,
                })
                .await;
        }

        let harness = Self {
            registry,
            session_store,
            thread_store,
        };
        wait_for_handle(&harness.slot(), Duration::from_secs(5))
            .await
            .expect("capture handle opened");
        harness
    }

    fn slot(&self) -> Arc<LhcCaptureSlot> {
        self.thread_store
            .get::<LhcCaptureSlot>()
            .expect("capture slot")
    }

    async fn stop_thread(&self) {
        for contributor in self.registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_stop(ThreadStopInput {
                    session_store: &self.session_store,
                    thread_store: &self.thread_store,
                })
                .await;
        }
    }
}

fn message(role: &str, text: &str, id: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::from_server(id.into())),
        role: role.into(),
        content: vec![if role == "assistant" {
            ContentItem::OutputText { text: text.into() }
        } else {
            ContentItem::InputText { text: text.into() }
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn thread_stop_is_bounded_when_capture_worker_is_blocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = LifecycleHarness::start(dir.path(), "blocked-thread-stop").await;
    let handle = harness.slot().get().expect("capture handle");
    let release = handle.block_worker().await;

    let stopped = tokio::time::timeout(Duration::from_secs(5), harness.stop_thread()).await;
    let _ = release.send(());

    assert!(
        stopped.is_ok(),
        "production thread-stop lifecycle must not wait indefinitely for a blocked capture worker"
    );
}

#[tokio::test]
async fn degraded_shutdown_reports_known_persistence_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = LifecycleHarness::start(dir.path(), "degraded-thread-stop").await;
    let handle = harness.slot().get().expect("capture handle");
    handle.latch_degraded("shutdown_test");

    let result = shutdown_capture_send(handle).await;

    assert_eq!(
        result,
        CaptureShutdownResult::Failed(CaptureShutdownFailure::PersistenceFailed),
        "degraded intake must never be reported as durably persisted"
    );
}

#[tokio::test]
async fn successful_thread_stop_persists_prompt_assistant_and_completed_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "durable-thread-stop";
    let turn_id = "turn-1";
    let harness = LifecycleHarness::start(root, thread_id).await;
    let turn_store = ExtensionData::new(turn_id);
    let collaboration_mode = CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model: "gpt-5.1".into(),
            reasoning_effort: None,
            developer_instructions: None,
        },
    };
    let token_usage = TokenUsage::default();

    for contributor in harness.registry.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(TurnStartInput {
                turn_id,
                collaboration_mode: &collaboration_mode,
                token_usage_at_turn_start: Some(&token_usage),
                started_at: Some(1_720_000_000),
                session_store: &harness.session_store,
                thread_store: &harness.thread_store,
                turn_store: &turn_store,
            })
            .await;
    }

    let items = [
        message("user", "durable shutdown prompt", "user-1"),
        message("assistant", "durable shutdown response", "assistant-1"),
    ];
    for contributor in harness.registry.raw_item_contributors() {
        contributor
            .on_raw_items(RawItemInput {
                items: &items[..1],
                provenance: RawItemProvenance::UserPrompt,
                session_store: &harness.session_store,
                thread_store: &harness.thread_store,
                turn_store: Some(&turn_store),
            })
            .await;
        contributor
            .on_raw_items(RawItemInput {
                items: &items[1..],
                provenance: RawItemProvenance::ModelOutput,
                session_store: &harness.session_store,
                thread_store: &harness.thread_store,
                turn_store: Some(&turn_store),
            })
            .await;
    }

    for contributor in harness.registry.turn_lifecycle_contributors() {
        contributor
            .on_turn_stop(TurnStopInput {
                started_at: Some(1_720_000_000),
                completed_at: Some(1_720_000_042),
                session_store: &harness.session_store,
                thread_store: &harness.thread_store,
                turn_store: &turn_store,
            })
            .await;
    }
    harness.stop_thread().await;

    let reopened = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("reopen capture");
    let events = reopened.list_events().await.expect("list events");
    let turns = reopened.list_turns().await.expect("list turns");

    let event_kinds = events
        .iter()
        .map(|event| event.event_kind().as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        event_kinds,
        vec!["user_prompt", "assistant_text", "turn_end"]
    );
    assert_eq!(
        turns
            .iter()
            .filter(|turn| {
                turn.status.as_str() == "closed"
                    && turn.member_message_ids.len() == 2
                    && turn.outcome.map(lhc::intake_stream::TurnOutcome::as_str)
                        == Some("completed")
            })
            .count(),
        1,
        "the host-completed turn must be durable after thread stop; turns={turns:?}"
    );
    reopened.shutdown().await;
}

fn pending_inference_callbacks() -> crate::InferenceCallbacks {
    use std::sync::Arc;
    fn hang() -> crate::BoxInferenceFuture {
        Box::pin(std::future::pending())
    }
    crate::InferenceCallbacks {
        smooth_prompt: Arc::new(|_| hang()),
        summarize_tool_result: Arc::new(|_| hang()),
        compress_detailed_turn: Arc::new(|_| hang()),
        summarize_chunk_brief: Arc::new(|_| hang()),
    }
}

fn claimed_work_item_count(path: &std::path::Path) -> i64 {
    let Some(path) = path.to_str() else {
        return 0;
    };
    if !std::path::Path::new(path).exists() {
        return 0;
    }
    let db = match lhc::shared_tech::storage::open_database(path) {
        lhc::shared_tech::errors::OpResult::Ok { value } => value,
        lhc::shared_tech::errors::OpResult::Err { .. } => return 0,
    };
    db.prepare("SELECT count(*) AS n FROM work_item WHERE status = 'claimed'")
        .get()
        .and_then(|row| {
            row.get("n")
                .and_then(|value| value.as_i64().or_else(|| value.as_u64().map(|n| n as i64)))
        })
        .unwrap_or(0)
}

async fn wait_for_claimed_work_item(path: &std::path::Path, bound: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        if claimed_work_item_count(path) > 0 {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn shutdown_ack_returns_while_seeded_provider_is_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "acked-provider-pending";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(pending_inference_callbacks()),
    )
    .await
    .expect("capture");
    handle.persist(
        &message("user", "pending provider prompt", "user-1"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    handle.persist(
        &message("assistant", "pending provider reply", "assistant-1"),
        RawItemProvenance::ModelOutput,
        /*step_index*/ None,
    );
    handle.flush().await;

    let path = crate::session::thread_file_path(root, thread_id);
    assert!(
        wait_for_claimed_work_item(&path, Duration::from_secs(2)).await,
        "seeded hanging inference must hold a claim before shutdown"
    );

    let wait = handle.clone();
    let started = std::time::Instant::now();
    let result = handle.shutdown_bounded(Duration::from_secs(2)).await;
    assert_eq!(
        result,
        CaptureShutdownResult::Persisted,
        "intake-durability ack must not wait for hanging inference"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "ack returned after {:?}, which waited on the provider",
        started.elapsed()
    );
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "cancelled derivation must let the capture runtime return within write+close settle"
    );
    assert_eq!(claimed_work_item_count(&path), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        claimed_work_item_count(&path),
        0,
        "a stopped runtime must not re-claim after hand-back"
    );
}

#[tokio::test]
async fn thread_stop_hands_back_only_after_pending_provider_runtime_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "stop-provider-pending";
    let harness = LifecycleHarness::start(root, thread_id).await;
    harness
        .slot()
        .set_derivation_callbacks(pending_inference_callbacks());
    let handle = harness.slot().get().expect("capture handle");
    handle.persist(
        &message("user", "pending stop prompt", "user-1"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    handle.persist(
        &message("assistant", "pending stop reply", "assistant-1"),
        RawItemProvenance::ModelOutput,
        /*step_index*/ None,
    );
    handle.flush().await;

    let path = crate::session::thread_file_path(root, thread_id);
    assert!(
        wait_for_claimed_work_item(&path, Duration::from_secs(2)).await,
        "hanging inference must hold a claim before thread stop"
    );

    let stopped = tokio::time::timeout(Duration::from_secs(10), harness.stop_thread()).await;
    assert!(
        stopped.is_ok(),
        "thread stop must stay bounded while inference is pending"
    );
    assert!(
        handle
            .wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "cancelled derivation must return the worker within write+close settle"
    );
    assert_eq!(claimed_work_item_count(&path), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        claimed_work_item_count(&path),
        0,
        "a stopped runtime must not requeue a claim after its own hand-back"
    );
}

#[tokio::test]
async fn terminated_state_is_visible_to_a_subscriber_that_arrives_after_completion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = spawn_capture(
        "complete-before-subscribe",
        None,
        Some(dir.path().to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("capture");
    let wait = handle.clone();
    handle.shutdown().await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while !wait.is_terminated() {
        assert!(
            Instant::now() < deadline,
            "idle shutdown must publish terminated without a live subscriber"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = Instant::now();
    assert!(
        wait.wait_terminated_bounded(Duration::from_secs(2)).await,
        "late subscribe must observe the retained terminated state"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "completion-before-subscribe must not wait the termination bound; elapsed {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn idle_thread_stop_does_not_wait_the_termination_bound() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = LifecycleHarness::start(dir.path(), "idle-clean-close").await;
    let started = Instant::now();
    harness.stop_thread().await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "idle clean close must not sit on the capture termination bound; elapsed {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn shutdown_timeout_with_live_worker_hands_back_once_after_return() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "timeout-live-worker";
    let harness = LifecycleHarness::start(root, thread_id).await;
    let handle = harness.slot().get().expect("capture handle");
    let path = crate::session::thread_file_path(root, thread_id);
    seed_held_claim(&path, "w-timeout");
    assert_eq!(claimed_work_item_count(&path), 1);

    let _release = handle.block_worker().await;
    let stopped = tokio::time::timeout(Duration::from_secs(5), harness.stop_thread()).await;
    assert!(
        stopped.is_ok(),
        "thread stop must return when shutdown preempts a blocked worker"
    );
    assert!(
        handle
            .wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "shutdown must preempt Block and return the worker within write+close settle"
    );
    assert_eq!(claimed_work_item_count(&path), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        claimed_work_item_count(&path),
        0,
        "hand-back must happen exactly once after the worker returns"
    );
}

#[tokio::test]
async fn former_owner_drop_does_not_take_resumed_thread_claim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "transfer-thread";
    let owner_a = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("owner A capture");
    let wait_a = owner_a.clone();
    owner_a.shutdown().await;
    assert!(
        wait_a.wait_terminated_bounded(Duration::from_secs(5)).await,
        "A must fully terminate before B resumes the same thread"
    );
    drop(wait_a);

    let owner_b = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(pending_inference_callbacks()),
    )
    .await
    .expect("owner B capture");
    owner_b.persist(
        &message("user", "resume derivation prompt", "user-resume"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    owner_b.persist(
        &message("assistant", "resume derivation reply", "assistant-resume"),
        RawItemProvenance::ModelOutput,
        /*step_index*/ None,
    );
    owner_b.flush().await;
    let path = crate::session::thread_file_path(root, thread_id);
    assert!(
        wait_for_claimed_work_item(&path, Duration::from_secs(2)).await,
        "B's resumed runtime must hold a real derivation claim"
    );

    // Former owner close is not a path-scoped sweep. Dropping A (already
    // terminated) must leave B's claim claimed.
    assert!(
        claimed_work_item_count(&path) > 0,
        "closing the former owner must not take the live successor's claim"
    );
    let wait_b = owner_b.clone();
    owner_b.shutdown().await;
    assert!(wait_b.wait_terminated_bounded(Duration::from_secs(8)).await);
    assert_eq!(claimed_work_item_count(&path), 0);
}

#[tokio::test]
async fn racing_successor_keeps_its_claim_until_predecessor_returns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "race-hb";
    let predecessor = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(pending_inference_callbacks()),
    )
    .await
    .expect("predecessor");
    predecessor.persist(
        &message("user", "predecessor claim prompt", "user-pred"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    predecessor.persist(
        &message("assistant", "predecessor claim reply", "assistant-pred"),
        RawItemProvenance::ModelOutput,
        /*step_index*/ None,
    );
    predecessor.flush().await;
    let path = crate::session::thread_file_path(root, thread_id);
    assert!(
        wait_for_claimed_work_item(&path, Duration::from_secs(2)).await,
        "predecessor must hold a claim before shutdown"
    );

    let wait_pred = predecessor.clone();
    let successor_fut = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(pending_inference_callbacks()),
    );
    tokio::pin!(successor_fut);
    let shutting = tokio::spawn(async move { predecessor.shutdown().await });
    let successor = tokio::select! {
        result = &mut successor_fut => {
            assert!(
                wait_pred.is_terminated(),
                "successor must not open before the predecessor returns"
            );
            result
        }
        ok = wait_pred.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND) => {
            assert!(
                ok,
                "predecessor must return within the worker-return bound"
            );
            tokio::time::timeout(Duration::from_secs(2), successor_fut)
                .await
                .expect("successor must open once the predecessor returns")
        }
    };
    shutting.await.expect("predecessor shutdown");
    let successor = successor.expect("successor capture");
    successor.persist(
        &message("user", "successor claim prompt", "user-succ"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    successor.persist(
        &message("assistant", "successor claim reply", "assistant-succ"),
        RawItemProvenance::ModelOutput,
        /*step_index*/ None,
    );
    successor.flush().await;
    assert!(
        wait_for_claimed_work_item(&path, Duration::from_secs(2)).await,
        "successor must hold its own claim after opening"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        claimed_work_item_count(&path) > 0,
        "predecessor hand-back must not requeue the successor's claim"
    );
    let wait_succ = successor.clone();
    successor.shutdown().await;
    assert!(
        wait_succ
            .wait_terminated_bounded(Duration::from_secs(8))
            .await
    );
}

#[tokio::test]
async fn cancelled_spawn_capture_releases_path_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let thread_id = "cancel-open-thread";
    let derivation = LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks"));

    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn(async move {
        let _guard = crate::session::hold_registry_lock_for_tests().await;
        let _ = locked_tx.send(());
        std::future::pending::<()>().await;
    });
    locked_rx.await.expect("registry lock held");

    let first = tokio::time::timeout(
        Duration::from_millis(200),
        spawn_capture(thread_id, None, Some(root.clone()), derivation.clone()),
    )
    .await;
    assert!(
        first.is_err(),
        "open must stay pending while the registry lock is held"
    );

    holder.abort();
    let _ = holder.await;

    let handle = tokio::time::timeout(
        Duration::from_secs(5),
        spawn_capture(thread_id, None, Some(root), derivation),
    )
    .await
    .expect("successor must not wait forever after a cancelled open")
    .expect("successor capture");
    let wait = handle.clone();
    handle.shutdown().await;
    assert!(wait.wait_terminated_bounded(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn shutdown_returns_within_bound_when_derivation_call_hangs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "r8-derivation-hang";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(pending_inference_callbacks()),
    )
    .await
    .expect("capture");
    handle.persist(
        &message("user", "hang derivation prompt", "user-1"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    handle.persist(
        &message("assistant", "hang derivation reply", "assistant-1"),
        RawItemProvenance::ModelOutput,
        /*step_index*/ None,
    );
    handle.flush().await;
    let path = crate::session::thread_file_path(root, thread_id);
    assert!(
        wait_for_claimed_work_item(&path, Duration::from_secs(2)).await,
        "hanging derivation must hold a claim"
    );
    let wait = handle.clone();
    let started = Instant::now();
    handle.shutdown().await;
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "worker stuck in a derivation call must return within write+close settle"
    );
    assert!(
        started.elapsed() < crate::capture::CAPTURE_WORKER_RETURN_BOUND,
        "elapsed {:?}",
        started.elapsed()
    );
    assert_eq!(claimed_work_item_count(&path), 0);
}

#[tokio::test]
async fn shutdown_returns_within_bound_when_resolve_is_unseeded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "r8-unseeded-resolve";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::new(),
    )
    .await
    .expect("capture");
    handle.persist(
        &message("user", "unseeded prompt", "user-1"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    handle.persist(
        &message("assistant", "unseeded reply", "assistant-1"),
        RawItemProvenance::ModelOutput,
        /*step_index*/ None,
    );
    handle.flush().await;
    let wait = handle.clone();
    let started = Instant::now();
    handle.shutdown().await;
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "worker stuck in unseeded resolve must return within write+close settle"
    );
    assert!(
        started.elapsed() < crate::capture::CAPTURE_WORKER_RETURN_BOUND,
        "elapsed {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn shutdown_returns_within_bound_when_persist_is_blocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "r8-blocked-persist";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("capture");
    handle.flush().await;
    let path = crate::session::thread_file_path(root, thread_id);
    let db = match lhc::shared_tech::storage::open_database(path.to_str().expect("utf-8")) {
        lhc::shared_tech::errors::OpResult::Ok { value } => value,
        lhc::shared_tech::errors::OpResult::Err { error } => panic!("{}", error.reason),
    };
    db.exec("BEGIN EXCLUSIVE");
    handle.persist(
        &message("user", "blocked persist prompt", "user-block"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    let wait = handle.clone();
    let started = Instant::now();
    handle.shutdown().await;
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "worker stuck in persist must return within write+close settle"
    );
    assert!(
        started.elapsed() < crate::capture::CAPTURE_WORKER_RETURN_BOUND,
        "elapsed {:?}",
        started.elapsed()
    );
    db.exec("ROLLBACK");
}

#[tokio::test]
async fn shutdown_returns_within_bound_when_queued_writes_are_blocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "r8-queued-writes";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("capture");
    handle.flush().await;
    let path = crate::session::thread_file_path(root, thread_id);
    let db = match lhc::shared_tech::storage::open_database(path.to_str().expect("utf-8")) {
        lhc::shared_tech::errors::OpResult::Ok { value } => value,
        lhc::shared_tech::errors::OpResult::Err { error } => panic!("{}", error.reason),
    };
    db.exec("BEGIN EXCLUSIVE");
    for index in 0..3 {
        handle.persist(
            &message(
                "user",
                &format!("queued persist {index}"),
                &format!("user-queued-{index}"),
            ),
            RawItemProvenance::UserPrompt,
            /*step_index*/ None,
        );
    }
    let wait = handle.clone();
    let started = Instant::now();
    let result = handle
        .shutdown_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
        .await;
    assert_eq!(
        result,
        CaptureShutdownResult::Failed(CaptureShutdownFailure::PersistenceFailed),
        "uncommitted queued writes must not be reported as Persisted"
    );
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "queued writes under a held lock must finish or fail within write+close settle"
    );
    assert!(
        started.elapsed() <= crate::capture::CAPTURE_WORKER_RETURN_BOUND + Duration::from_secs(1),
        "elapsed {:?}",
        started.elapsed()
    );
    db.exec("ROLLBACK");
}

#[tokio::test]
async fn shutdown_returns_within_bound_when_active_flush_is_blocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "r8-active-flush";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("capture");
    handle.flush().await;
    let path = crate::session::thread_file_path(root, thread_id);
    let db = match lhc::shared_tech::storage::open_database(path.to_str().expect("utf-8")) {
        lhc::shared_tech::errors::OpResult::Ok { value } => value,
        lhc::shared_tech::errors::OpResult::Err { error } => panic!("{}", error.reason),
    };
    db.exec("BEGIN EXCLUSIVE");
    for index in 0..3 {
        handle.persist(
            &message(
                "assistant",
                &format!("flush output {index}"),
                &format!("asst-flush-{index}"),
            ),
            RawItemProvenance::ModelOutput,
            /*step_index*/ None,
        );
    }
    handle.flush_async();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let wait = handle.clone();
    let started = Instant::now();
    let result = handle.shutdown_bounded(Duration::from_secs(11)).await;
    assert_eq!(
        result,
        CaptureShutdownResult::Failed(CaptureShutdownFailure::PersistenceFailed),
        "an in-flight flush under a held lock must not report Persisted"
    );
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "active flush under a held lock must return within the worker bound"
    );
    assert!(
        started.elapsed() < Duration::from_secs(11),
        "active flush must not run past 11s; elapsed {:?}",
        started.elapsed()
    );
    db.exec("ROLLBACK");
}

#[cfg(target_os = "linux")]
fn capture_os_thread_alive(thread_id: &str) -> bool {
    let mut comm = format!("lhc-capture-{thread_id}");
    comm.truncate(15);
    std::fs::read_dir("/proc/self/task")
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            std::fs::read_to_string(entry.path().join("comm"))
                .ok()
                .is_some_and(|name| name.trim() == comm)
        })
}

#[tokio::test]
async fn shutdown_releases_reservation_within_bound_when_flush_and_handback_are_blocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "cf";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(pending_inference_callbacks()),
    )
    .await
    .expect("capture");
    handle.persist(
        &message("user", "hold a real derivation claim", "user-claim"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    handle.flush().await;
    let path = crate::session::thread_file_path(root, thread_id);
    assert!(
        wait_for_claimed_work_item(&path, Duration::from_secs(2)).await,
        "seeded hanging inference must hold a claim"
    );
    let db = match lhc::shared_tech::storage::open_database(path.to_str().expect("utf-8")) {
        lhc::shared_tech::errors::OpResult::Ok { value } => value,
        lhc::shared_tech::errors::OpResult::Err { error } => panic!("{}", error.reason),
    };
    db.exec("BEGIN EXCLUSIVE");
    for index in 0..3 {
        handle.persist(
            &message(
                "assistant",
                &format!("flush output {index}"),
                &format!("asst-cf-{index}"),
            ),
            RawItemProvenance::ModelOutput,
            /*step_index*/ None,
        );
    }
    handle.flush_async();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let wait = handle.clone();
    let started = Instant::now();
    let result = handle
        .shutdown_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
        .await;
    assert_eq!(
        result,
        CaptureShutdownResult::Failed(CaptureShutdownFailure::PersistenceFailed)
    );
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "reservation must be released by the worker-return bound"
    );
    #[cfg(target_os = "linux")]
    {
        while capture_os_thread_alive(thread_id)
            && started.elapsed() <= crate::capture::CAPTURE_WORKER_RETURN_BOUND
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !capture_os_thread_alive(thread_id),
            "capture OS thread must be gone by the bound; elapsed {:?}",
            started.elapsed()
        );
    }
    assert!(
        started.elapsed() <= crate::capture::CAPTURE_WORKER_RETURN_BOUND,
        "capture thread and reservation outlived the bound; elapsed {:?}",
        started.elapsed()
    );
    db.exec("ROLLBACK");
    let successor = tokio::time::timeout(
        Duration::from_secs(2),
        spawn_capture(
            thread_id,
            None,
            Some(root.to_path_buf()),
            LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
        ),
    )
    .await
    .expect("successor must not wait on a leaked reservation");
    let successor = successor.expect("successor must open after reservation release");
    successor.shutdown().await;
}

#[tokio::test]
async fn drain_settled_aborts_when_shutdown_is_requested() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = spawn_capture(
        "ds",
        None,
        Some(dir.path().to_path_buf()),
        LateBoundCallbacks::seeded(pending_inference_callbacks()),
    )
    .await
    .expect("capture");
    handle.persist(
        &message("user", "pending drain", "user-drain"),
        RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );
    handle.flush().await;
    let wait = handle.clone();
    let draining = tokio::spawn(async move { wait.drain_settled(Duration::from_secs(120)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    handle.shutdown().await;
    let settled = tokio::time::timeout(Duration::from_secs(2), draining)
        .await
        .expect("DrainSettled must not ignore shutdown")
        .expect("drain task");
    assert!(!settled, "shutdown must abort DrainSettled as unsettled");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "DrainSettled ignored shutdown; elapsed {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn dropping_last_handle_releases_admission_for_a_successor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let thread_id = "r10-last-handle-drop";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.clone()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("capture");
    drop(handle);
    let successor = tokio::time::timeout(
        Duration::from_secs(2),
        spawn_capture(
            thread_id,
            None,
            Some(root),
            LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
        ),
    )
    .await;
    let successor = successor.expect("successor must not wait on a leaked reservation");
    let successor = successor.expect("successor must open after last handle drop");
    successor.shutdown().await;
}

#[tokio::test]
async fn shutdown_drains_queued_model_change_through_the_command_handler() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let thread_id = "r11-model-change";
    let handle = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("capture");
    let release = handle.block_worker().await;
    handle.model_or_thinking_change("old", "new", "low", "high");
    let wait = handle.clone();
    let shutting =
        tokio::spawn(async move { handle.shutdown_bounded(Duration::from_secs(5)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = release.send(());
    let result = shutting.await.expect("join");
    assert_eq!(result, CaptureShutdownResult::Persisted);
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await
    );
    let reopened = spawn_capture(
        thread_id,
        None,
        Some(root.to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("reopen");
    let kinds = reopened
        .list_events()
        .await
        .expect("list events")
        .iter()
        .map(|event| event.event_kind().as_str().to_string())
        .collect::<Vec<_>>();
    assert!(
        kinds.iter().any(|kind| kind == "model_change"),
        "queued model change must be committed; kinds={kinds:?}"
    );
    assert!(
        kinds.iter().any(|kind| kind == "thinking_level_change"),
        "queued thinking-level change must be committed; kinds={kinds:?}"
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn shutdown_none_with_a_live_handle_does_not_wait_the_deadline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = spawn_capture(
        "r12-shutdown-none",
        None,
        Some(dir.path().to_path_buf()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("capture");
    let wait = handle.clone();
    let started = Instant::now();
    handle.shutdown_async();
    assert!(
        wait.wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
            .await,
        "Shutdown(None) must still retire the worker"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "Shutdown(None) must not wait the full return deadline; elapsed {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn racing_thread_starts_loser_gets_retry_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let thread_id = "r8-race-admission";
    let start = |root: std::path::PathBuf| async move {
        let mut builder = ExtensionRegistryBuilder::<()>::new();
        install_with_root(&mut builder, |_config| true, root);
        let registry = builder.build();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new(thread_id);
        let config = ();
        let session_source = SessionSource::Exec;
        let environments = [];
        for contributor in registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_start(ThreadStartInput {
                    config: &config,
                    session_source: &session_source,
                    persistent_thread_state_available: false,
                    environments: &environments,
                    mcp_resource_client: None,
                    extension_metrics: None,
                    session_store: &session_store,
                    thread_store: &thread_store,
                })
                .await;
        }
        let slot = thread_store.get::<LhcCaptureSlot>().expect("capture slot");
        (registry, session_store, thread_store, slot.state())
    };
    let ((registry_a, session_a, thread_a, state_a), (registry_b, session_b, thread_b, state_b)) =
        tokio::join!(start(root.clone()), start(root));
    let retry = |state: &CaptureState| matches!(state, CaptureState::Failed(reason) if reason == CAPTURE_STILL_SHUTTING_DOWN);
    assert!(
        retry(&state_a) ^ retry(&state_b),
        "exactly one starter must fail with the retry error; a={state_a:?} b={state_b:?}"
    );
    let winner = if retry(&state_a) {
        (&registry_b, &session_b, &thread_b)
    } else {
        (&registry_a, &session_a, &thread_a)
    };
    let slot = winner.2.get::<LhcCaptureSlot>().expect("winner slot");
    wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("winner capture opened");
    for contributor in winner.0.thread_lifecycle_contributors() {
        contributor
            .on_thread_stop(ThreadStopInput {
                session_store: winner.1,
                thread_store: winner.2,
            })
            .await;
    }
}

#[tokio::test]
async fn successor_open_fails_when_predecessor_does_not_return() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let thread_id = "r8-admission-backstop";
    let owner_a = spawn_capture(
        thread_id,
        None,
        Some(root.clone()),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await
    .expect("owner A");
    let _release = owner_a.block_worker().await;
    let started = Instant::now();
    let owner_b = spawn_capture(
        thread_id,
        None,
        Some(root),
        LateBoundCallbacks::seeded(lhc_inference_callbacks(false).expect("callbacks")),
    )
    .await;
    assert!(
        owner_b.is_none(),
        "successor must fail instead of waiting forever"
    );
    assert!(
        started.elapsed() >= crate::capture::CAPTURE_ADMISSION_BOUND,
        "admission timeout must wait the bound; elapsed {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < crate::capture::CAPTURE_ADMISSION_BOUND + Duration::from_secs(2),
        "admission timeout must not hang past the bound; elapsed {:?}",
        started.elapsed()
    );
    let wait_a = owner_a.clone();
    owner_a.shutdown().await;
    let _ = wait_a
        .wait_terminated_bounded(crate::capture::CAPTURE_WORKER_RETURN_BOUND)
        .await;
}

fn seed_held_claim(path: &std::path::Path, work_item_id: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("threads dir");
    let db = match lhc::shared_tech::storage::open_database(path.to_str().expect("utf-8")) {
        lhc::shared_tech::errors::OpResult::Ok { value } => value,
        lhc::shared_tech::errors::OpResult::Err { error } => panic!("{}", error.reason),
    };
    db.exec(&format!(
        "CREATE TABLE IF NOT EXISTS work_item (
            work_item_id TEXT PRIMARY KEY,
            owner TEXT NOT NULL,
            kind TEXT NOT NULL,
            source_ref TEXT NOT NULL,
            status TEXT NOT NULL,
            queued_at TEXT NOT NULL,
            claimed_at TEXT,
            claim_expires_at TEXT,
            payload TEXT NOT NULL
         );
         INSERT OR REPLACE INTO work_item (
            work_item_id, owner, kind, source_ref, status, queued_at,
            claimed_at, claim_expires_at, payload
         ) VALUES (
            '{work_item_id}', 'test', 'test', 'test', 'claimed', 'now',
            'now', 'later', '{{\"claimAttempt\":1}}'
         );"
    ));
    lhc::shared_tech::work_queue::note_claim_held(
        &db,
        &lhc::shared_tech::work_queue::ClaimAttempt {
            work_item_id: work_item_id.into(),
            claim_attempt: Some(1),
        },
    );
    db.close();
}
