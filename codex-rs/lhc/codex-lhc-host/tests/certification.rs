//! Chunk 1 certification — production-path invariants under rule zero.
//!
//! Rule zero: every test that certifies what LHC *records* submits through a
//! real `LhcSession` (via `spawn_capture` / install path) and **reads the
//! stored row back**. Mapper-only assertions may supplement a round-trip test
//! but never stand alone as the sole guard.
//!
//! Run: `cargo test -p codex-lhc-host --features test-util --test certification`

#![cfg(feature = "test-util")]

use std::path::PathBuf;
use std::time::Duration;

// Trait imports required for method resolution on dyn contributors.
#[allow(unused_imports)]
use codex_extension_api::ConfigContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
#[allow(unused_imports)]
use codex_extension_api::RawItemContributor;
use codex_extension_api::RawItemInput;
use codex_extension_api::RawItemProvenance;
#[allow(unused_imports)]
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_lhc_host::CAPTURE_QUEUE_CAP;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::LhcSession;
use codex_lhc_host::OccurrenceTracker;
use codex_lhc_host::install_with_root;
use codex_lhc_host::install_with_root_and_labels;
use codex_lhc_host::item_digest;
use codex_lhc_host::item_event_key;
use codex_lhc_host::map_item;
use codex_lhc_host::seed_occurrence_from_keys;
use codex_lhc_host::spawn_capture;
use codex_lhc_host::wait_for_handle;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::models::LocalShellStatus;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

/// Minimal config view for ConfigContributor tests.
#[derive(Clone, Debug, PartialEq)]
struct FakeConfig {
    model: String,
    effort: String,
}

fn user_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::from_server(format!(
            "msg_{}",
            text.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        ))),
        role: "user".into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn all_variant_fixtures() -> Vec<(&'static str, ResponseItem, RawItemProvenance)> {
    vec![
        (
            "message_user",
            user_msg("hello from user"),
            RawItemProvenance::UserPrompt,
        ),
        (
            "message_assistant",
            ResponseItem::Message {
                id: Some(ResponseItemId::from_server("msg_asst".into())),
                role: "assistant".into(),
                content: vec![ContentItem::OutputText {
                    text: "hello from assistant".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "message_developer",
            ResponseItem::Message {
                id: None,
                role: "developer".into(),
                content: vec![ContentItem::InputText {
                    text: "dev policy".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::HostContext,
        ),
        (
            "host_context_user_role",
            ResponseItem::Message {
                id: Some(ResponseItemId::from_server("msg_ctx".into())),
                role: "user".into(),
                content: vec![ContentItem::InputText {
                    text: "RolloutBudgetContext".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::HostContext,
        ),
        (
            "agent_message",
            ResponseItem::AgentMessage {
                id: None,
                author: "agent-a".into(),
                recipient: "agent-b".into(),
                content: vec![
                    codex_protocol::models::AgentMessageInputContent::InputText {
                        text: "handoff".into(),
                    },
                ],
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::InterAgent,
        ),
        (
            "reasoning_summary",
            ResponseItem::Reasoning {
                id: Some(ResponseItemId::from_server("rs_1".into())),
                summary: vec![ReasoningItemReasoningSummary::SummaryText {
                    text: "thinking aloud".into(),
                }],
                content: None,
                encrypted_content: None,
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "reasoning_encrypted",
            ResponseItem::Reasoning {
                id: Some(ResponseItemId::from_server("rs_enc".into())),
                summary: vec![],
                content: None,
                encrypted_content: Some("OPAQUE_ENCRYPTED_BYTES_verbatim".into()),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "local_shell_call",
            ResponseItem::LocalShellCall {
                id: None,
                call_id: Some("shell-1".into()),
                status: LocalShellStatus::Completed,
                action: LocalShellAction::Exec(LocalShellExecAction {
                    command: vec!["echo".into(), "hi".into()],
                    timeout_ms: None,
                    working_directory: None,
                    env: None,
                    user: None,
                }),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "function_call",
            ResponseItem::FunctionCall {
                id: Some(ResponseItemId::from_server("fc_1".into())),
                name: "read_file".into(),
                namespace: None,
                arguments: r#"{"path":"src/main.rs"}"#.into(),
                call_id: "fc_1".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "function_call_output",
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "fc_1".into(),
                output: FunctionCallOutputPayload::from_text("fn main() {}".into()),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "tool_search_call",
            ResponseItem::ToolSearchCall {
                id: None,
                call_id: Some("ts_1".into()),
                status: Some("completed".into()),
                execution: "server".into(),
                arguments: serde_json::json!({"query": "web"}),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "tool_search_output",
            ResponseItem::ToolSearchOutput {
                id: None,
                call_id: Some("ts_1".into()),
                tools: vec![serde_json::json!({"name": "web_search"})],
                status: "completed".into(),
                execution: "server".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "custom_tool_call",
            ResponseItem::CustomToolCall {
                id: Some(ResponseItemId::from_server("ctc_1".into())),
                status: None,
                call_id: "ctc_1".into(),
                name: "custom".into(),
                input: r#"{"x":1}"#.into(),
                namespace: None,
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "custom_tool_call_output",
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "ctc_1".into(),
                name: None,
                output: FunctionCallOutputPayload::from_text("ok".into()),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "web_search_call",
            ResponseItem::WebSearchCall {
                id: Some(ResponseItemId::from_server("ws_1".into())),
                status: Some("completed".into()),
                action: Some(WebSearchAction::Search {
                    query: Some("rust async".into()),
                    queries: None,
                }),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "image_generation_call",
            ResponseItem::ImageGenerationCall {
                id: Some(ResponseItemId::from_server("ig_1".into())),
                status: "completed".into(),
                revised_prompt: Some("a cat".into()),
                result: "base64…".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::ModelOutput,
        ),
        (
            "additional_tools",
            ResponseItem::AdditionalTools {
                id: None,
                role: "system".into(),
                tools: vec![serde_json::json!({"name": "extra"})],
            },
            RawItemProvenance::HostContext,
        ),
        (
            "compaction",
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "compaction-ciphertext".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::HostContext,
        ),
        (
            "compaction_trigger",
            ResponseItem::CompactionTrigger {},
            RawItemProvenance::HostContext,
        ),
        (
            "context_compaction",
            ResponseItem::ContextCompaction {
                id: None,
                encrypted_content: Some("ctx-compact-blob".into()),
                internal_chat_message_metadata_passthrough: None,
            },
            RawItemProvenance::HostContext,
        ),
        ("other", ResponseItem::Other, RawItemProvenance::HostContext),
    ]
}

/// Goldens are derived from mapper output but only after a real LhcSession
/// round-trip has accepted the same events (rule zero). Mapper comparison is
/// an addition, not the sole guard.
#[tokio::test]
async fn mapping_goldens_round_trip_and_match_fixtures() {
    let goldens_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../goldens");
    std::fs::create_dir_all(&goldens_dir).expect("goldens dir");
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();

    for (name, item, provenance) in all_variant_fixtures() {
        let mut tracker = OccurrenceTracker::new();
        let mapped = map_item("golden-thread", &item, provenance, &mut tracker);

        // Round-trip through real LhcSession and read stored rows back.
        let tid = format!("golden-rt-{name}");
        let handle = spawn_capture(
            &tid,
            None,
            Some(root.clone()),
            codex_lhc_host::LateBoundCallbacks::seeded(
                codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
            ),
        )
        .await
        .expect("spawn");
        handle.persist(&item, provenance);
        handle.flush().await;
        let stored = handle.list_events().await.expect("list");
        handle.shutdown().await;

        assert_eq!(
            stored.len(),
            mapped.len(),
            "{name}: stored rows must match mapped event count (LHC must accept every event)"
        );
        for (i, (ev, m)) in stored.iter().zip(mapped.iter()).enumerate() {
            assert_eq!(
                ev.event_kind().as_str(),
                m.input.event_kind.as_str(),
                "{name}[{i}]: kind"
            );
            // No envelope `extra` leakage: stored payload must carry host raw
            // data if the mapper put it in arguments.__hostRaw.
            if let Some(args) = m.input.payload.get("arguments") {
                if let Some(raw) = args.get("__hostRaw") {
                    let tp = ev.tool_call_payload().expect("tool_call payload");
                    assert_eq!(
                        tp.arguments.get("__hostRaw"),
                        Some(raw),
                        "{name}[{i}]: __hostRaw must round-trip inside payload"
                    );
                }
            }
            assert!(
                m.input.extra.is_empty(),
                "{name}[{i}]: extra must stay empty (H1); envelope rejects unknown keys"
            );
        }

        let rendered: Vec<serde_json::Value> = mapped
            .iter()
            .map(|e| {
                serde_json::json!({
                    "event_kind": e.input.event_kind,
                    "actor": e.input.actor,
                    "harness": e.input.harness,
                    "payload": e.input.payload,
                    "extra": e.input.extra,
                    "idempotency_key": e.input.idempotency_key,
                })
            })
            .collect();
        let body = serde_json::to_string_pretty(&rendered).expect("json");
        let path = goldens_dir.join(format!("{name}.json"));
        if std::env::var("UPDATE_LHC_GOLDENS").ok().as_deref() == Some("1") {
            std::fs::write(&path, format!("{body}\n")).expect("write golden");
            continue;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("missing golden {path:?}: {err}. Run with UPDATE_LHC_GOLDENS=1");
        });
        assert_eq!(expected.trim(), body.trim(), "golden mismatch for {name}");
    }
}

/// H1: function_call arguments raw bytes survive envelope validation into storage.
#[tokio::test]
async fn arguments_raw_byte_exact_round_trip() {
    let cases = [
        r#"{"n":1e3}"#,
        r#"{ "a" : 1 ,  "b" : 2 }"#,
        r#"{"k":1,"k":2}"#,
        "42",
    ];
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    for (i, raw) in cases.iter().enumerate() {
        let item = ResponseItem::FunctionCall {
            id: Some(ResponseItemId::from_server(format!("fc_raw_{i}"))),
            name: "x".into(),
            namespace: None,
            arguments: (*raw).into(),
            call_id: format!("c{i}"),
            internal_chat_message_metadata_passthrough: None,
        };
        let handle = spawn_capture(
            &format!("raw-bytes-{i}"),
            None,
            Some(root.clone()),
            codex_lhc_host::LateBoundCallbacks::seeded(
                codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
            ),
        )
        .await
        .expect("spawn");
        handle.persist(&item, RawItemProvenance::ModelOutput);
        handle.flush().await;
        let events = handle.list_events().await.expect("list");
        assert_eq!(events.len(), 1, "raw={raw}");
        let args = &events[0].tool_call_payload().expect("tool_call").arguments;
        assert_eq!(
            args.get("__hostRaw"),
            Some(&serde_json::json!(raw)),
            "verbatim wire string must round-trip inside payload.arguments.__hostRaw; raw={raw}"
        );
        handle.shutdown().await;
    }
}

/// H1: full image URL is stored (not truncated, not rejected).
#[tokio::test]
async fn image_url_full_round_trip() {
    let url = format!("data:image/png;base64,{}", "A".repeat(500));
    let item = ResponseItem::Message {
        id: Some(ResponseItemId::from_server("msg_img".into())),
        role: "user".into(),
        content: vec![ContentItem::InputImage {
            image_url: url.clone(),
            detail: None,
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let dir = tempdir().unwrap();
    let handle = spawn_capture(
        "img-full",
        None,
        Some(dir.path().to_path_buf()),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    handle.persist(&item, RawItemProvenance::UserPrompt);
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert_eq!(events.len(), 1);
    let text = &events[0].text_payload().expect("text").text;
    assert!(
        text.contains(&url),
        "full image URL must be in stored text, got len={}",
        text.len()
    );
    handle.shutdown().await;
}

/// F1: open, capture, restart, re-present same id → still one row.
#[tokio::test]
async fn production_restart_replay_records_once() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let thread_id = "prod-restart-thread";
    let item = user_msg("one human utterance");

    let h1 = spawn_capture(
        thread_id,
        None,
        Some(root.clone()),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    h1.persist(&item, RawItemProvenance::UserPrompt);
    h1.flush().await;
    let before = h1.list_events().await.expect("list").len();
    assert_eq!(before, 1);
    h1.shutdown().await;

    let h2 = spawn_capture(
        thread_id,
        None,
        Some(root.clone()),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("reopen");
    h2.persist(&item, RawItemProvenance::UserPrompt);
    h2.flush().await;
    let after = h2.list_events().await.expect("list").len();
    assert_eq!(
        after, 1,
        "restart re-presentation of same ResponseItemId must not double-record (before={before})"
    );
    h2.shutdown().await;
}

/// Two genuinely distinct occurrences of identical text get distinct ids → two records.
#[tokio::test]
async fn distinct_ids_same_text_record_twice() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let handle = spawn_capture(
        "distinct-ids",
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    let a = user_msg("same text");
    let mut b = user_msg("same text");
    if let ResponseItem::Message { id, .. } = &mut b {
        *id = Some(ResponseItemId::from_server("msg_second_occ".into()));
    }
    handle.persist(&a, RawItemProvenance::UserPrompt);
    handle.persist(&b, RawItemProvenance::UserPrompt);
    handle.flush().await;
    let n = handle.list_events().await.expect("list").len();
    assert_eq!(n, 2, "distinct ResponseItemIds must both record");
    handle.shutdown().await;
}

/// F2/H8: crash after partial submit; injection points 0, 1, 2.
/// Also covers same-kind multi-event (reasoning summary+encrypted) part-suffix (H15).
#[tokio::test]
async fn crash_partial_submit_then_retry_no_double() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();

    // ImageGenerationCall: 2 events, different kinds (tool_call + tool_result).
    let image_item = ResponseItem::ImageGenerationCall {
        id: Some(ResponseItemId::from_server("ig_probe".into())),
        status: "completed".into(),
        revised_prompt: Some("cat".into()),
        result: "b64".into(),
        internal_chat_message_metadata_passthrough: None,
    };
    // Reasoning: 2 events, same kind (assistant_thinking) with part suffixes.
    let reasoning_item = ResponseItem::Reasoning {
        id: Some(ResponseItemId::from_server("rs_multi".into())),
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: "summary part".into(),
        }],
        content: None,
        encrypted_content: Some("encrypted-part".into()),
        internal_chat_message_metadata_passthrough: None,
    };

    for (label, item, expected) in [
        ("image", image_item, 2usize),
        ("reasoning", reasoning_item, 2usize),
    ] {
        // Parameterize over injection points 0, 1, 2 (before / between / after-all).
        for after in [0usize, 1, 2] {
            let tid = format!("crash-{label}-{after}");
            let h = spawn_capture(
                &tid,
                None,
                Some(root.clone()),
                codex_lhc_host::LateBoundCallbacks::seeded(
                    codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
                ),
            )
            .await
            .expect("spawn");
            h.arm_crash_mid_persist(after).await;
            h.persist(&item, RawItemProvenance::ModelOutput);
            // ModelOutput is buffered until flush/provider_usage/turn_end so
            // assistant_text can carry providerUsage; flush forces the crash.
            let _ = h.flush().await;
            tokio::time::sleep(Duration::from_millis(100)).await;

            let h2 = spawn_capture(
                &tid,
                None,
                Some(root.clone()),
                codex_lhc_host::LateBoundCallbacks::seeded(
                    codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
                ),
            )
            .await
            .expect("respawn");
            h2.persist(&item, RawItemProvenance::ModelOutput);
            h2.flush().await;
            let events = h2.list_events().await.expect("list");
            assert_eq!(
                events.len(),
                expected,
                "label={label} after={after}: must end with exactly {expected} events, got {}",
                events.len()
            );
            h2.shutdown().await;
        }
    }
}

#[tokio::test]
async fn abort_turn_emits_turn_end() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let handle = spawn_capture(
        "abort-turn",
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    handle.persist(&user_msg("start"), RawItemProvenance::UserPrompt);
    handle.turn_end(
        "turn-1",
        "aborted",
        codex_lhc_host::TurnEndFacts {
            outcome: Some("aborted"),
            outcome_reason: Some("interrupted".into()),
            started_at: None,
            ended_at: None,
        },
    );
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert!(
        events.iter().any(|e| e.event_kind().as_str() == "turn_end"),
        "abort must emit turn_end"
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn flag_off_installs_no_capture() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    install_with_root(&mut builder, |_c: &()| false, root.clone());
    let registry = builder.build();

    let session_store = ExtensionData::new("session-flag-off");
    let thread_store = ExtensionData::new("thread-flag-off");
    let environments = [];
    let source = SessionSource::Exec;
    for c in registry.thread_lifecycle_contributors() {
        c.on_thread_start(ThreadStartInput {
            config: &(),
            session_source: &source,
            persistent_thread_state_available: false,
            environments: &environments,
            mcp_resource_client: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    }
    assert!(
        thread_store.get::<LhcCaptureSlot>().is_none(),
        "flag off must not install a capture slot"
    );
}

#[tokio::test]
async fn flag_on_captures_via_raw_item_contributor() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    install_with_root(&mut builder, |_c: &()| true, root);
    let registry = builder.build();

    let session_store = ExtensionData::new("session-on");
    let thread_store = ExtensionData::new("thread-on");
    let environments = [];
    let source = SessionSource::Exec;
    for c in registry.thread_lifecycle_contributors() {
        c.on_thread_start(ThreadStartInput {
            config: &(),
            session_source: &source,
            persistent_thread_state_available: false,
            environments: &environments,
            mcp_resource_client: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    }
    let slot = thread_store
        .get::<LhcCaptureSlot>()
        .expect("flag on must install slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");
    let items = [user_msg("via contributor")];
    for c in registry.raw_item_contributors() {
        c.on_raw_items(RawItemInput {
            items: &items,
            provenance: RawItemProvenance::UserPrompt,
            session_store: &session_store,
            thread_store: &thread_store,
            turn_store: None,
        })
        .await;
    }
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert_eq!(events.len(), 1);
    handle.shutdown().await;
}

/// H2: items arriving before handle is ready must not be dropped.
/// Does **not** call wait_for_handle before submitting.
#[tokio::test]
async fn pre_open_items_are_buffered_not_dropped() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    install_with_root(&mut builder, |_c: &()| true, root);
    let registry = builder.build();

    let session_store = ExtensionData::new("session-race");
    let thread_store = ExtensionData::new("thread-race");
    let environments = [];
    let source = SessionSource::Exec;
    for c in registry.thread_lifecycle_contributors() {
        c.on_thread_start(ThreadStartInput {
            config: &(),
            session_source: &source,
            persistent_thread_state_available: false,
            environments: &environments,
            mcp_resource_client: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    }
    // Immediately submit — do not wait for handle (the race under test).
    let items = [user_msg("first-prompt"), user_msg("second-prompt")];
    for c in registry.raw_item_contributors() {
        c.on_raw_items(RawItemInput {
            items: &items,
            provenance: RawItemProvenance::UserPrompt,
            session_store: &session_store,
            thread_store: &thread_store,
            turn_store: None,
        })
        .await;
    }
    let slot = thread_store
        .get::<LhcCaptureSlot>()
        .expect("slot installed");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle eventually ready");
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    let prompts: Vec<_> = events
        .iter()
        .filter(|e| e.event_kind().as_str() == "user_prompt")
        .collect();
    assert_eq!(
        prompts.len(),
        2,
        "pre-open buffer must retain both early prompts; got {} events {:?}",
        events.len(),
        events
            .iter()
            .map(|e| e.event_kind().as_str())
            .collect::<Vec<_>>()
    );
    handle.shutdown().await;
}

/// H4: force queue saturation deterministically (block worker, overfill).
/// Assertions are unconditional — no if-guarded branch.
#[tokio::test]
async fn queue_full_latches_degraded_and_counts_drops() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let handle = spawn_capture(
        "queue-full",
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");

    // Park the worker so the queue cannot drain.
    let release = handle.block_worker().await;

    // Fill beyond user capacity (one slot reserved for degradation note).
    let n = CAPTURE_QUEUE_CAP + 32;
    for i in 0..n {
        let item = ResponseItem::Message {
            id: Some(ResponseItemId::from_server(format!("msg_flood_{i}"))),
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: format!("flood {i}"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        handle.persist(&item, RawItemProvenance::UserPrompt);
    }

    let dropped = handle.dropped_count();
    assert!(
        dropped > 0,
        "blocking the worker then overfilling must drop; dropped={dropped}"
    );
    assert!(
        handle.is_degraded(),
        "any drop must latch degraded (dropped={dropped})"
    );

    // Release and flush; truncation note must be in the stored record (H6).
    let _ = release.send(());
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert!(
        events.iter().any(|e| {
            e.event_kind().as_str() == "runtime_note"
                && e.text_payload()
                    .is_some_and(|p| p.text.contains("degraded"))
        }),
        "degraded latch must leave a self-describing runtime_note in the record; got {:?}",
        events
            .iter()
            .map(|e| e.event_kind().as_str())
            .collect::<Vec<_>>()
    );
    handle.shutdown().await;
}

/// H5: same id + evolved content (status advance) records both versions.
#[tokio::test]
async fn status_advance_same_id_records_both() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let handle = spawn_capture(
        "status-advance",
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    let in_progress = ResponseItem::ImageGenerationCall {
        id: Some(ResponseItemId::from_server("ig_evolve".into())),
        status: "in_progress".into(),
        revised_prompt: None,
        result: String::new(),
        internal_chat_message_metadata_passthrough: None,
    };
    let completed = ResponseItem::ImageGenerationCall {
        id: Some(ResponseItemId::from_server("ig_evolve".into())),
        status: "completed".into(),
        revised_prompt: Some("done".into()),
        result: "b64-final".into(),
        internal_chat_message_metadata_passthrough: None,
    };
    handle.persist(&in_progress, RawItemProvenance::ModelOutput);
    handle.persist(&completed, RawItemProvenance::ModelOutput);
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    // Each ImageGenerationCall maps to tool_call + tool_result → 4 events.
    assert_eq!(
        events.len(),
        4,
        "in_progress and completed must both record (id+digest keys); got {}",
        events.len()
    );
    let contents: Vec<_> = events
        .iter()
        .filter_map(|e| e.tool_result_payload().map(|p| p.content.clone()))
        .collect();
    assert!(
        contents.iter().any(|c| c.contains("in_progress")),
        "stored rows must include in_progress body: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .any(|c| c.contains("completed") && c.contains("b64-final")),
        "stored rows must include completed body: {contents:?}"
    );
    handle.shutdown().await;
}

/// H15: anonymous path is an unreachable defensive fallback for host-assigned
/// variants. CompactionTrigger/Other map to nothing; we still exercise the
/// seed path with deliberately id-less Message fixtures.
#[tokio::test]
async fn open_seeds_occurrence_from_stored_anon_keys() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let thread_id = "seed-prod-path";
    // Deliberately id-less — host normally assigns ids; this path is a
    // defensive fallback documented as unreachable for mappable host items.
    let item = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "anon text".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let h1 = spawn_capture(
        thread_id,
        None,
        Some(root.clone()),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    h1.persist(&item, RawItemProvenance::UserPrompt);
    h1.persist(&item, RawItemProvenance::UserPrompt);
    h1.flush().await;
    assert_eq!(h1.list_events().await.unwrap().len(), 2);
    h1.shutdown().await;

    let h2 = spawn_capture(
        thread_id,
        None,
        Some(root.clone()),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("reopen");
    h2.persist(&item, RawItemProvenance::UserPrompt);
    h2.flush().await;
    let events = h2.list_events().await.unwrap();
    assert_eq!(
        events.len(),
        3,
        "seeded high-water must allocate occ=2 for third presentation"
    );
    h2.shutdown().await;
}

/// Permanent disable after poison + truncation note (H6).
#[tokio::test]
async fn capture_disabled_after_poison() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, _) = LhcSession::open(
        "poison-thread",
        None,
        Some(root.as_path()),
        codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
    )
    .await
    .expect("open");
    session.poison();
    let r = session
        .submit_events(&[lhc::intake_stream::MessageEventInput {
            event_kind: "user_prompt".into(),
            idempotency_key: Some("k".into()),
            actor: "user".into(),
            harness: "codex".into(),
            payload: serde_json::Map::new(),
            extra: serde_json::Map::new(),
        }])
        .await;
    assert!(r.is_err());
    assert!(session.capture_disabled);
}

#[test]
fn seed_from_keys_is_restart_stable() {
    let item = user_msg("stable");
    let d = item_digest(&item);
    let k0 = item_event_key("tid", None, &d, 0, "user_prompt", None);
    let k1 = item_event_key("tid", None, &d, 1, "user_prompt", None);
    let mut t = seed_occurrence_from_keys([k0.as_str(), k1.as_str()]);
    assert_eq!(t.next(&d), 2);
}

/// G1/H7: ConfigContributor emits transition-stable model/thinking events.
/// Same prev→new re-fire collides; genuinely new transition appends.
#[tokio::test]
async fn config_change_emits_model_and_thinking_events() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut builder = ExtensionRegistryBuilder::<FakeConfig>::new();
    install_with_root_and_labels(
        &mut builder,
        |_c| true,
        root,
        |c: &FakeConfig| c.model.clone(),
        |c: &FakeConfig| c.effort.clone(),
        |_c| None,
    );
    let registry = builder.build();

    let session_store = ExtensionData::new("s-cfg");
    let thread_store = ExtensionData::new("t-cfg");
    let environments = [];
    let source = SessionSource::Exec;
    let start_cfg = FakeConfig {
        model: "gpt-a".into(),
        effort: "low".into(),
    };
    for c in registry.thread_lifecycle_contributors() {
        c.on_thread_start(ThreadStartInput {
            config: &start_cfg,
            session_source: &source,
            persistent_thread_state_available: false,
            environments: &environments,
            mcp_resource_client: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    }
    let slot = thread_store.get::<LhcCaptureSlot>().expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    let prev = FakeConfig {
        model: "gpt-a".into(),
        effort: "low".into(),
    };
    let next = FakeConfig {
        model: "gpt-b".into(),
        effort: "high".into(),
    };
    for c in registry.config_contributors() {
        c.on_config_changed(&session_store, &thread_store, &prev, &next);
    }
    // No-op config change must emit nothing.
    for c in registry.config_contributors() {
        c.on_config_changed(&session_store, &thread_store, &next, &next);
    }
    // Re-fire of the same transition must not double-record (transition keys).
    for c in registry.config_contributors() {
        c.on_config_changed(&session_store, &thread_store, &prev, &next);
    }
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    let kinds: Vec<_> = events.iter().map(|e| e.event_kind().as_str()).collect();
    assert!(
        kinds.contains(&"model_change"),
        "expected model_change, got {kinds:?}"
    );
    assert!(
        kinds.contains(&"thinking_level_change"),
        "expected thinking_level_change, got {kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "model_change" || **k == "thinking_level_change")
            .count(),
        2,
        "re-fire of same transition must collide; got {kinds:?}"
    );
    handle.shutdown().await;
}

/// H7: transition keys are restart-stable — re-firing the same prev→new after
/// reopen collides; a genuine new transition appends.
#[tokio::test]
async fn model_change_transition_keys_restart_stable() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let thread_id = "model-restart";
    let h1 = spawn_capture(
        thread_id,
        None,
        Some(root.clone()),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    h1.model_or_thinking_change("m1", "m2", "none", "high");
    h1.flush().await;
    let first = h1.list_events().await.unwrap();
    assert_eq!(first.len(), 2);
    let first_keys: Vec<_> = first
        .iter()
        .map(|e| e.idempotency_key().to_string())
        .collect();
    h1.shutdown().await;

    let h2 = spawn_capture(
        thread_id,
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("reopen");
    // Same transition re-fire (resume) — must collide.
    h2.model_or_thinking_change("m1", "m2", "none", "high");
    h2.flush().await;
    let mid = h2.list_events().await.unwrap();
    assert_eq!(mid.len(), 2, "same transition re-fire must not append");
    // Genuinely new transition.
    h2.model_or_thinking_change("m2", "m3", "high", "high");
    h2.flush().await;
    let after = h2.list_events().await.unwrap();
    assert_eq!(after.len(), 3, "new model_change on top of two stored");
    let new_keys: Vec<_> = after
        .iter()
        .map(|e| e.idempotency_key().to_string())
        .filter(|k| !first_keys.contains(k))
        .collect();
    assert_eq!(new_keys.len(), 1);
    assert!(
        new_keys[0].contains(":model_change:m2:m3"),
        "expected transition key m2→m3, got {}",
        new_keys[0]
    );
    h2.shutdown().await;
}

/// H9: a panicking map path must not kill the worker for later items.
/// We cannot easily inject a panic into map_item without a test hook; instead
/// verify the worker continues after a normal item and that catch_unwind is
/// present in the capture source (structural) + a multi-item persist sequence
/// remains healthy after a deliberately odd item.
#[tokio::test]
async fn worker_survives_many_items() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let handle = spawn_capture(
        "worker-survive",
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    for i in 0..20 {
        handle.persist(
            &user_msg(&format!("item-{i}")),
            RawItemProvenance::UserPrompt,
        );
    }
    // Other maps to nothing — must not break the worker.
    handle.persist(&ResponseItem::Other, RawItemProvenance::HostContext);
    handle.persist(&user_msg("after-other"), RawItemProvenance::UserPrompt);
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert_eq!(events.len(), 21);
    assert!(!handle.is_degraded());
    handle.shutdown().await;
}

/// H9/I3: map_item panic must not kill the worker — subsequent items still
/// reach storage (behavioural; not an include_str of catch_unwind).
#[tokio::test]
async fn worker_survives_map_item_panic_and_records_later_items() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    // Sentinel thread id triggers a one-shot panic inside map_item (test-util).
    let handle = spawn_capture(
        "__lhc_test_panic_map__",
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic"),
        ),
    )
    .await
    .expect("spawn");
    handle.persist(
        &user_msg("this-map-panics-once"),
        RawItemProvenance::UserPrompt,
    );
    handle.persist(&user_msg("after-map-panic"), RawItemProvenance::UserPrompt);
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert_eq!(
        events.len(),
        1,
        "first item panics in map; second must still record; got {:?}",
        events
            .iter()
            .map(|e| e.event_kind().as_str())
            .collect::<Vec<_>>()
    );
    let text = events[0]
        .text_payload()
        .map(|p| p.text.as_str())
        .unwrap_or("");
    assert!(
        text.contains("after-map-panic"),
        "stored row must be the post-panic item; got {text:?}"
    );
    assert!(!handle.is_degraded());
    handle.shutdown().await;
}

/// Round 11: a settle-wait that cannot finish must neither hang the caller nor
/// wedge the capture worker.
///
/// Unseeded `LateBoundCallbacks` make this reachable deterministically: the
/// user prompt queues a `smoothed_prompt` derivation, the background scheduler
/// claims it, and its handler parks on a callback resolve that never comes —
/// the thread can never settle. That is exactly the production window between
/// `on_thread_start` and the host seeding real callbacks.
#[tokio::test]
async fn unsettleable_drain_neither_hangs_caller_nor_wedges_worker() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let handle = spawn_capture(
        "unsettleable-drain",
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::new(), // deliberately never seeded
    )
    .await
    .expect("spawn");
    handle.persist(
        &user_msg("a prompt that queues smoothing work"),
        RawItemProvenance::UserPrompt,
    );
    handle.flush().await;

    // 1. The caller's wait is bounded even though the thread can never settle.
    let settled = handle.drain_settled(Duration::from_millis(400)).await;
    assert!(
        !settled,
        "unseeded derivation can never settle; reporting settled would mean \
         the wait is not actually gated on the scheduler"
    );

    // 2. The worker is not wedged behind that wait: commands queued after it
    //    must still run. If the settle-wait were awaited unbounded on the
    //    worker, this flush (and Shutdown below) would never be processed.
    //    Bounded so a wedged worker fails the test instead of hanging it.
    handle.persist(&user_msg("second prompt"), RawItemProvenance::UserPrompt);
    tokio::time::timeout(Duration::from_secs(10), handle.flush())
        .await
        .expect("worker must process a flush queued behind a timed-out settle-wait");
    let events = handle.list_events().await.expect("list");
    assert!(
        events.len() >= 2,
        "worker must keep recording after a timed-out settle-wait; got {}",
        events.len()
    );

    // 3. Shutdown returns promptly: unseeded work is provably unsettleable, so
    //    close skips the settle-wait instead of sitting out its bound.
    tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
        .await
        .expect("shutdown must not hang on unsettleable derivation");
}

/// Round 11: even with callbacks seeded, shutdown must be bounded when
/// derivation hangs (a wedged model call in production). `LhcSession::close`
/// waits for quiescence only up to its bound, then abandons the claim to the
/// durable queue — a hung inference call must never turn shutdown into a hang.
#[tokio::test]
async fn shutdown_is_bounded_when_seeded_derivation_hangs() {
    use std::sync::Arc;

    fn hang<T>() -> Arc<
        dyn Fn(T) -> lhc::shared_tech::derivation::BoxFuture<lhc::shared_tech::InferenceResult>
            + Send
            + Sync,
    >
    where
        T: 'static,
    {
        Arc::new(|_input: T| Box::pin(std::future::pending()))
    }
    let hanging = lhc::shared_tech::InferenceCallbacks {
        smooth_prompt: hang(),
        summarize_tool_result: hang(),
        compress_detailed_turn: hang(),
        summarize_chunk_brief: hang(),
    };

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let handle = spawn_capture(
        "seeded-hung-derivation",
        None,
        Some(root),
        codex_lhc_host::LateBoundCallbacks::seeded(hanging),
    )
    .await
    .expect("spawn");
    handle.persist(
        &user_msg("a prompt whose smoothing call hangs forever"),
        RawItemProvenance::UserPrompt,
    );
    handle.flush().await;

    // Well over close's settle bound (5s) but far under "hang": if this trips,
    // close is waiting on quiescence without a bound again.
    tokio::time::timeout(Duration::from_secs(20), handle.shutdown())
        .await
        .expect("shutdown must be bounded while a seeded derivation call hangs");
}
