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
use codex_extension_api::ThreadLifecycleContributor;
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
    let handle2 = codex_lhc_host::spawn_capture(&thread_id, None, Some(root))
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
    let orders: Vec<i64> = events.iter().map(|e| e.event_order()).collect();
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
    let h2 = codex_lhc_host::spawn_capture(&thread_id, None, Some(root))
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
        kinds.iter().any(|k| *k == "runtime_note"),
        "model-output path must not classify user-role as user_prompt; got {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| *k == "user_prompt"),
        "user-role item via record_completed_response_item must not be user_prompt (would mean UserPrompt tag); got {kinds:?}"
    );
    handle.shutdown().await;
}
