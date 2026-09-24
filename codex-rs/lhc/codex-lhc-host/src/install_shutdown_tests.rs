//! Production lifecycle coverage for capture shutdown durability and bounds.

use super::*;

use crate::LateBoundCallbacks;
use crate::lhc_inference_callbacks;
use crate::spawn_capture;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::RawItemInput;
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
        !wait.is_terminated(),
        "capture runtime must still be inside close while the provider is pending"
    );
    assert!(
        claimed_work_item_count(&path) > 0,
        "claim must stay held until the capture runtime stops"
    );

    assert!(
        wait.wait_terminated_bounded(Duration::from_secs(8)).await,
        "close settle bound must still terminate the capture runtime"
    );
    crate::handback::on_thread_unload(Some(root), thread_id);
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
    assert_eq!(claimed_work_item_count(&path), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        claimed_work_item_count(&path),
        0,
        "thread stop must not requeue a claim under a still-running worker"
    );
}
