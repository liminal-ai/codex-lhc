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
                token_usage_at_turn_start: &token_usage,
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
