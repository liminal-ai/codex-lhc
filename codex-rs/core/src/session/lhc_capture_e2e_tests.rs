//! End-to-end LHC capture through the real `Session` recording seam (F11/F14).
//!
//! These tests fail when the raw-item contributor invocation is deleted from
//! `send_raw_response_items` while leaving comments/sentinels intact.
//!
//! Rule zero for core behavior: exercise production paths (id assignment,
//! provenance tags, config fan-out, panic containment) — not hand-built
//! registries alone.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_extension_api::ConfigContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::RawItemContributor;
use codex_extension_api::RawItemInput;
use codex_extension_api::RawItemProvenance;
use codex_extension_api::ThreadStartInput;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::install_with_root;
use codex_lhc_host::wait_for_handle;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::Session;
use super::tests::make_session_and_context;
use crate::config::Config;

fn text_input(text: &str) -> UserInput {
    UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }
}

async fn install_lhc_on_session(session: &mut Session, root: std::path::PathBuf) {
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
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

#[tokio::test]
async fn e2e_user_prompt_reaches_lhc_record() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root).await;

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("LhcCaptureSlot installed by on_thread_start");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("capture handle opened");

    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            &[text_input("e2e human utterance")],
            None,
        )
        .await;

    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert!(
        !events.is_empty(),
        "LHC record must contain the user prompt; empty means the core hook is disconnected"
    );
    assert!(
        events
            .iter()
            .any(|e| e.event_kind().as_str() == "user_prompt"),
        "expected user_prompt, got {:?}",
        events
            .iter()
            .map(|e| e.event_kind().as_str())
            .collect::<Vec<_>>()
    );
    handle.shutdown().await;
}

/// H11: capture → close → reopen → assert prompts appear in original order
/// *by identity*, without pre-sorting the stored stream.
#[tokio::test]
async fn e2e_item_order_preserved_across_records() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root.clone()).await;

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    let texts = [
        "first-prompt-identity",
        "second-prompt-identity",
        "third-prompt-identity",
    ];
    for text in texts {
        session
            .record_user_prompt_and_emit_turn_item(&turn_context, &[text_input(text)], None)
            .await;
    }
    handle.flush().await;
    handle.shutdown().await;

    // Close + reopen the LHC thread (process-restart analogue).
    let thread_id = session.thread_id().to_string();
    let handle2 = codex_lhc_host::spawn_capture(&thread_id, None, Some(root), Default::default())
        .await
        .expect("reopen");
    let events = handle2.list_events().await.expect("list after reopen");

    // Do NOT sort. Extract prompt texts in stream order and compare by identity.
    let prompt_texts: Vec<String> = events
        .iter()
        .filter(|e| e.event_kind().as_str() == "user_prompt")
        .filter_map(|e| e.text_payload().map(|p| p.text.clone()))
        .collect();
    assert_eq!(
        prompt_texts,
        texts.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
        "prompts must appear in original order by identity (no pre-sort)"
    );
    let orders: Vec<i64> = events.iter().map(codex_lhc_host::EventRecord::event_order).collect();
    assert!(
        orders.windows(2).all(|w| w[0] <= w[1]),
        "raw list_events order must be non-decreasing: {orders:?}"
    );
    handle2.shutdown().await;
}

/// H12: real id assignment via prepare_conversation_items_for_history.
/// A regression where core mints fresh ids on every presentation of the same
/// logical item would break restart stability; this certifies prepare mints a
/// durable id and re-presenting that id collides in LHC.
#[tokio::test]
async fn e2e_core_id_assignment_is_restart_stable() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root.clone()).await;

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    // Drive the production record path (assigns id at history boundary).
    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            &[text_input("core-assigned-id-utterance")],
            None,
        )
        .await;
    handle.flush().await;
    let first = handle.list_events().await.expect("list");
    assert_eq!(first.len(), 1);
    let first_key = first[0].idempotency_key().to_string();
    assert!(
        first_key.contains(":id:"),
        "core must assign a ResponseItemId so keys are id-primary; got {first_key}"
    );
    handle.shutdown().await;

    // prepare_conversation_items_for_history is the production mint path.
    let items = [ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "another-mint".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let prepared = session
        .prepare_conversation_items_for_history(&turn_context, &items)
        .into_owned();
    let id_str = prepared[0]
        .id()
        .map(|i| i.as_str().to_string())
        .expect("prepare must assign id");
    assert!(!id_str.is_empty(), "minted id must be non-empty");

    let thread_id = session.thread_id().to_string();
    let h2 = codex_lhc_host::spawn_capture(&thread_id, None, Some(root), Default::default())
        .await
        .expect("reopen");
    // Re-present the same prepared item twice — must collide.
    h2.persist(&prepared[0], RawItemProvenance::UserPrompt);
    h2.persist(&prepared[0], RawItemProvenance::UserPrompt);
    h2.flush().await;
    let after = h2.list_events().await.expect("list");
    // First open already recorded one user_prompt; the prepared item is new.
    // Count only events whose key carries this id.
    let matching = after
        .iter()
        .filter(|e| e.idempotency_key().contains(&id_str))
        .count();
    assert_eq!(
        matching,
        1,
        "same prepared ResponseItemId must record once; keys={:?}",
        after
            .iter()
            .map(|e| e.idempotency_key().to_string())
            .collect::<Vec<_>>()
    );
    h2.shutdown().await;
}

/// H10: provenance tags at real call sites.
#[tokio::test]
async fn e2e_user_prompt_provenance_is_required() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root).await;

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            &[text_input("provenance-sensitive")],
            None,
        )
        .await;
    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert!(
        events
            .iter()
            .any(|e| e.event_kind().as_str() == "user_prompt"),
        "record_user_prompt must tag UserPrompt so mapper emits user_prompt; got {:?}",
        events
            .iter()
            .map(|e| e.event_kind().as_str())
            .collect::<Vec<_>>()
    );
    // HostContext-tagged user-role must NOT become user_prompt.
    let host_item = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "host scaffolding".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    session
        .record_conversation_items_with_provenance(
            &turn_context,
            &[host_item],
            RawItemProvenance::HostContext,
        )
        .await;
    handle.flush().await;
    let events2 = handle.list_events().await.expect("list");
    let host_notes: Vec<_> = events2
        .iter()
        .filter(|e| {
            e.event_kind().as_str() == "runtime_note"
                && e.text_payload()
                    .is_some_and(|p| p.text.contains("host scaffolding"))
        })
        .collect();
    assert_eq!(
        host_notes.len(),
        1,
        "HostContext user-role must be runtime_note, not user_prompt"
    );
    handle.shutdown().await;
}

/// Counting ConfigContributor for H13.
struct CountingConfig {
    hits: Arc<AtomicUsize>,
}

impl ConfigContributor<Config> for CountingConfig {
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
        _previous_config: &Config,
        _new_config: &Config,
    ) {
        self.hits.fetch_add(1, Ordering::SeqCst);
    }
}

/// H13: config change must go through Session::emit_config_changed_contributors
/// (via `update_settings`). Replacing the fan-out with `.take(0)` fails this.
#[tokio::test]
async fn e2e_config_change_via_session_fanout() {
    use super::SessionSettingsUpdate;
    use codex_protocol::protocol::AskForApproval;

    let hits = Arc::new(AtomicUsize::new(0));
    let (mut session, _turn_context) = make_session_and_context().await;

    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    builder.config_contributor(Arc::new(CountingConfig {
        hits: Arc::clone(&hits),
    }));
    let dir = tempdir().expect("tempdir");
    install_with_root(&mut builder, |_c| true, dir.path().to_path_buf());
    session.services.extensions = Arc::new(builder.build());

    // Production fan-out: update_settings → emit_config_changed_contributors.
    session
        .update_settings(SessionSettingsUpdate {
            approval_policy: Some(AskForApproval::Never),
            ..Default::default()
        })
        .await
        .expect("update settings");

    assert!(
        hits.load(Ordering::SeqCst) >= 1,
        "Session::update_settings must fan out to config_contributors (H13); hits=0 means emit was gutted"
    );
}

/// H14: catch_unwind around raw-item contributors — a panicking contributor
/// must not abort session recording.
/// Stall risk: accepted. Contributors are try_send-only by contract; no
/// timeout is applied around the contributor future (documented in
/// send_raw_response_items).
#[tokio::test]
async fn e2e_panicking_raw_item_contributor_is_contained() {
    struct Boom;
    impl RawItemContributor for Boom {
        fn on_raw_items<'a>(
            &'a self,
            _input: RawItemInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                panic!("deliberate raw-item contributor panic for H14");
            })
        }
    }

    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, turn_context) = make_session_and_context().await;

    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    builder.raw_item_contributor(Arc::new(Boom));
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

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            &[text_input("after-panic-contributor")],
            None,
        )
        .await;

    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert!(
        events
            .iter()
            .any(|e| e.event_kind().as_str() == "user_prompt"),
        "recording must continue after a panicking contributor; got {:?}",
        events
            .iter()
            .map(|e| e.event_kind().as_str())
            .collect::<Vec<_>>()
    );
    handle.shutdown().await;
}

/// H10/I2: drive the real model-output recording path
/// (`record_completed_response_item`) against a live LHC session.
///
/// Provenance only changes the map for **user-role** items:
/// `UserPrompt` → `user_prompt`; `ModelOutput`/`HostContext`/`InterAgent` →
/// `runtime_note`. Assistant items map via role alone, so they cannot certify
/// the tag. A user-role item through the model-output path must become
/// `runtime_note` (fails if the path wrongly uses `UserPrompt`).
///
/// **Gap (honest):** `ModelOutput` vs `HostContext` vs `InterAgent` produce
/// identical stored shapes for every variant today — the precise tag on
/// stream/compact sites is not observable in LHC rows. Compaction's
/// `OutputItemDone` path is the same mapping; not separately driven here.
#[tokio::test]
async fn e2e_model_output_path_does_not_tag_user_role_as_user_prompt() {
    use crate::stream_events_utils::record_completed_response_item;

    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root).await;

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    // User-role item through the primary model-output recording path.
    let user_shaped = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "should-not-be-user-prompt-via-model-path".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    record_completed_response_item(&session, &turn_context, &user_shaped).await;

    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    let kinds: Vec<_> = events.iter().map(|e| e.event_kind().as_str()).collect();
    assert!(
        kinds.contains(&"runtime_note"),
        "model-output path must not classify user-role as user_prompt; got {kinds:?}"
    );
    assert!(
        !kinds.contains(&"user_prompt"),
        "user-role item via record_completed_response_item must not be user_prompt (would mean UserPrompt tag); got {kinds:?}"
    );
    handle.shutdown().await;
}

/// Chunk 3 / C1.2 — the invariant that makes resume and fork safe for LHC.
///
/// Rollout reconstruction (`apply_rollout_reconstruction` →
/// `state.replace_history`) installs history **without** going through
/// `send_raw_response_items`, so a resumed or forked session does not feed the
/// replayed history — including a previous compact's served body — back into
/// the archive as fresh source events.
///
/// That is load-bearing and invisible: if an upstream change ever routes
/// reconstruction through the record path, every resume silently re-ingests
/// LHC's own summary, and the next compact compounds it. Nothing else in the
/// fork would notice, because the resumed session's slot has no derived
/// provenance yet (I2's reseed runs at compact time, after reconstruction).
///
/// Written against the real reconstruction entry, not a stand-in.
#[tokio::test]
async fn e2e_rollout_reconstruction_does_not_re_ingest_into_capture() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("LhcCaptureSlot installed by on_thread_start");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("capture handle opened");

    // One genuinely captured turn, so "archive is empty" cannot pass by accident.
    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            &[text_input("live turn that must be captured")],
            None,
        )
        .await;
    handle.flush().await;
    let baseline = handle.list_events().await.expect("list").len();
    assert!(
        baseline > 0,
        "positive control: live turns must be captured"
    );

    // Now reconstruct history from rollout, as resume and fork do.
    let replayed: Vec<ResponseItem> = (0..6)
        .map(|i| ResponseItem::Message {
            id: Some(codex_protocol::ResponseItemId::from_server(format!(
                "rr{i}"
            ))),
            role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
            content: vec![if i % 2 == 0 {
                ContentItem::InputText {
                    text: format!("replayed rollout item {i}"),
                }
            } else {
                ContentItem::OutputText {
                    text: format!("replayed rollout reply {i}"),
                }
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })
        .collect();
    let rollout_items: Vec<codex_protocol::protocol::RolloutItem> = replayed
        .iter()
        .cloned()
        .map(codex_protocol::protocol::RolloutItem::ResponseItem)
        .collect();

    // The production entry itself — the same call `InitialHistory::Resumed`
    // and `InitialHistory::Forked` make in `Session::new`.
    let _ = session
        .apply_rollout_reconstruction(&turn_context, &rollout_items)
        .await;

    // Reconstruction is synchronous into state; give any stray async capture
    // path a chance to land before declaring nothing was ingested.
    handle.flush().await;
    let after = handle.list_events().await.expect("list").len();

    assert_eq!(
        after, baseline,
        "rollout reconstruction must not fan replayed history into capture: \
         archive grew {baseline} -> {after}. On a real resume that means the \
         previous compact's served body is re-ingested as source, and every \
         later compact re-summarises LHC's own output."
    );
    assert_eq!(
        session.clone_history().await.raw_items().len(),
        replayed.len(),
        "positive control: reconstruction must actually have installed history"
    );
    handle.shutdown().await;
}

/// Slice A — schema v5 host facts land in the LHC record through production
/// seams: ModelOutput record path + TokenUsageContributor + turn lifecycle.
///
/// Asserts all three facts (outcome/timing on turn_end, providerUsage on
/// assistant_text) and the abort path; also that omitting host facts still
/// records a valid turn_end (fields optional).
#[tokio::test]
async fn e2e_v5_host_facts_complete_and_provider_usage() {
    use crate::stream_events_utils::record_completed_response_item;
    use codex_extension_api::TurnStartInput;
    use codex_extension_api::TurnStopInput;
    use codex_protocol::protocol::TokenUsage;

    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root).await;

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    // Turn start through the real lifecycle contributor fan-out.
    let collaboration_mode = turn_context.collaboration_mode();
    let token_usage_at_start = TokenUsage::default();
    let started_at = 1_720_000_000_i64;
    let completed_at = 1_720_000_042_i64;
    for contributor in session.services.extensions.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(TurnStartInput {
                turn_id: turn_context.sub_id.as_str(),
                collaboration_mode: &collaboration_mode,
                token_usage_at_turn_start: &token_usage_at_start,
                started_at: Some(started_at),
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
            })
            .await;
    }

    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            &[text_input("v5 host facts prompt")],
            None,
        )
        .await;

    let assistant = ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::from_server(
            "asst-v5".into(),
        )),
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: "assistant reply with usage".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    // Production model-output path (ModelOutput provenance).
    record_completed_response_item(&session, &turn_context, &assistant).await;

    // ResponseEvent::Completed path — TokenUsageContributor fans out last_token_usage.
    let per_call = TokenUsage {
        input_tokens: 111,
        cached_input_tokens: 22,
        cache_write_input_tokens: 0,
        output_tokens: 33,
        reasoning_output_tokens: 4,
        total_tokens: 170,
    };
    session
        .record_token_usage_info(&turn_context, Some(&per_call))
        .await
        .expect("record token usage");

    for contributor in session.services.extensions.turn_lifecycle_contributors() {
        contributor
            .on_turn_stop(TurnStopInput {
                started_at: Some(started_at),
                completed_at: Some(completed_at),
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
            })
            .await;
    }

    handle.flush().await;
    let events = handle.list_events().await.expect("list events");

    let assistant_ev = events
        .iter()
        .find(|e| e.event_kind().as_str() == "assistant_text")
        .expect("assistant_text event");
    let usage = assistant_ev
        .assistant_text_payload()
        .and_then(|p| p.provider_usage.clone())
        .expect("providerUsage on assistant_text");
    assert_eq!(usage.get("input_tokens"), Some(&serde_json::json!(111)));
    assert_eq!(usage.get("output_tokens"), Some(&serde_json::json!(33)));
    assert_eq!(
        usage.get("cached_input_tokens"),
        Some(&serde_json::json!(22))
    );
    assert_eq!(
        usage.get("reasoning_output_tokens"),
        Some(&serde_json::json!(4))
    );

    let turn_end = events
        .iter()
        .find(|e| e.event_kind().as_str() == "turn_end")
        .expect("turn_end event");
    let payload = turn_end.turn_end_payload().expect("turn_end payload");
    assert_eq!(
        payload.outcome.as_ref().map(|o| o.as_str()),
        Some("completed")
    );
    assert_eq!(
        payload.started_at.as_deref(),
        Some("2024-07-03T09:46:40.000Z")
    );
    assert_eq!(
        payload.ended_at.as_deref(),
        Some("2024-07-03T09:47:22.000Z")
    );

    // Projected turns surface (rule zero: stored row, not just intake event).
    let turns = handle.list_turns().await.expect("list turns");
    let closed = turns
        .iter()
        .find(|t| t.status.as_str() == "closed")
        .expect("closed turn");
    assert_eq!(
        closed.outcome.as_ref().map(|o| o.as_str()),
        Some("completed")
    );
    assert_eq!(
        closed.started_at.as_deref(),
        Some("2024-07-03T09:46:40.000Z")
    );
    assert_eq!(closed.ended_at.as_deref(), Some("2024-07-03T09:47:22.000Z"));

    handle.shutdown().await;
}

#[tokio::test]
async fn e2e_v5_host_facts_abort_with_reason() {
    use codex_extension_api::TurnAbortInput;
    use codex_extension_api::TurnStartInput;
    use codex_protocol::protocol::TokenUsage;
    use codex_protocol::protocol::TurnAbortReason;

    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root).await;

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    let collaboration_mode = turn_context.collaboration_mode();
    let token_usage_at_start = TokenUsage::default();
    let started_at = 1_720_000_100_i64;
    let completed_at = 1_720_000_110_i64;
    for contributor in session.services.extensions.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(TurnStartInput {
                turn_id: turn_context.sub_id.as_str(),
                collaboration_mode: &collaboration_mode,
                token_usage_at_turn_start: &token_usage_at_start,
                started_at: Some(started_at),
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
            })
            .await;
    }

    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            &[text_input("abort path prompt")],
            None,
        )
        .await;

    for contributor in session.services.extensions.turn_lifecycle_contributors() {
        contributor
            .on_turn_abort(TurnAbortInput {
                reason: TurnAbortReason::Interrupted,
                started_at: Some(started_at),
                completed_at: Some(completed_at),
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
            })
            .await;
    }

    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    let turn_end = events
        .iter()
        .find(|e| e.event_kind().as_str() == "turn_end")
        .expect("turn_end");
    let payload = turn_end.turn_end_payload().expect("payload");
    assert_eq!(
        payload.outcome.as_ref().map(|o| o.as_str()),
        Some("aborted")
    );
    assert_eq!(payload.outcome_reason.as_deref(), Some("interrupted"));
    assert!(payload.started_at.is_some());
    assert!(payload.ended_at.is_some());

    let turns = handle.list_turns().await.expect("list turns");
    let closed = turns
        .iter()
        .find(|t| t.status.as_str() == "closed")
        .expect("closed turn");
    assert_eq!(closed.outcome.as_ref().map(|o| o.as_str()), Some("aborted"));
    assert_eq!(closed.outcome_reason.as_deref(), Some("interrupted"));

    handle.shutdown().await;
}

/// Optional fields: a turn_end with no host facts still records and closes.
#[tokio::test]
async fn e2e_v5_turn_end_without_host_facts_still_records() {
    use codex_extension_api::TurnStartInput;
    use codex_extension_api::TurnStopInput;
    use codex_protocol::protocol::TokenUsage;

    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, turn_context) = make_session_and_context().await;
    install_lhc_on_session(&mut session, root).await;

    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(5))
        .await
        .expect("handle");

    let collaboration_mode = turn_context.collaboration_mode();
    let token_usage_at_start = TokenUsage::default();
    for contributor in session.services.extensions.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(TurnStartInput {
                turn_id: turn_context.sub_id.as_str(),
                collaboration_mode: &collaboration_mode,
                token_usage_at_turn_start: &token_usage_at_start,
                started_at: None,
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
            })
            .await;
    }

    session
        .record_user_prompt_and_emit_turn_item(
            &turn_context,
            &[text_input("optional facts prompt")],
            None,
        )
        .await;

    // on_turn_stop always supplies completed when called via install, so drive
    // the capture handle directly with empty facts to pin optional semantics.
    handle.turn_end(
        turn_context.sub_id.as_str(),
        "completed",
        codex_lhc_host::TurnEndFacts::default(),
    );
    // Also exercise stop with timestamps omitted through the contributor
    // (started_at/completed_at None) — install still sets outcome completed.
    for contributor in session.services.extensions.turn_lifecycle_contributors() {
        contributor
            .on_turn_stop(TurnStopInput {
                started_at: None,
                completed_at: None,
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
            })
            .await;
    }

    handle.flush().await;
    let events = handle.list_events().await.expect("list");
    assert!(
        events
            .iter()
            .any(|e| e.event_kind().as_str() == "user_prompt"),
        "user prompt must still record without host facts"
    );
    let empty_end = events.iter().find(|e| {
        e.event_kind().as_str() == "turn_end"
            && e.turn_end_payload()
                .is_some_and(|p| p.outcome.is_none() && p.started_at.is_none())
    });
    assert!(
        empty_end.is_some(),
        "empty turn_end payload must remain valid; events={:?}",
        events
            .iter()
            .filter(|e| e.event_kind().as_str() == "turn_end")
            .map(|e| e.turn_end_payload().cloned())
            .collect::<Vec<_>>()
    );

    handle.shutdown().await;
}
