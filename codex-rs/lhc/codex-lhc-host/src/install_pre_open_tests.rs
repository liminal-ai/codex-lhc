//! Pre-open buffer handoff tests: ordered identity replay and overflow
//! degradation (A2 rounds 3–4).

use super::*;
use crate::capture::spawn_capture_with_identity;
use crate::inference::LateBoundCallbacks;
use crate::inference::lhc_inference_callbacks;
use crate::mapping::ModelIdentity;
use crate::parse_rollout_items;
use crate::rollout_reconcile::RolloutReconcileTrigger;
use crate::rollout_reconcile::regenerate_rollout_from_thread;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use tempfile::tempdir;

fn reasoning(id: &str, ciphertext: &str) -> ResponseItem {
    ResponseItem::Reasoning {
        id: Some(ResponseItemId::from_server(id.into())),
        summary: vec![],
        content: None,
        encrypted_content: Some(ciphertext.into()),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn user_msg(text: &str, id: &str) -> ResponseItem {
    use codex_protocol::models::ContentItem;
    ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.into())),
        role: "user".into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn open_handle(root: &std::path::Path, tid: &str, identity: ModelIdentity) -> CaptureHandle {
    let derivation = LateBoundCallbacks::new();
    derivation.seed(lhc_inference_callbacks(false).unwrap());
    spawn_capture_with_identity(
        tid,
        None,
        Some(root.to_path_buf()),
        derivation,
        Some(identity),
    )
    .await
    .expect("capture")
}

/// Pre-open commands (persist under identity A, SetIdentity(B), persist)
/// replay IN ORDER through set_and_flush: the first reasoning keeps
/// identity A, the second gets B. Black-box proof via regenerate: each
/// live identity re-emits exactly its own ciphertext.
#[tokio::test]
async fn pre_open_identity_change_replays_in_order() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "pre-open-order-tid";
    let id_a = ModelIdentity::new("openai", "gpt-a", ModelIdentity::RESPONSES_API);
    let id_b = ModelIdentity::new("openai", "gpt-b", ModelIdentity::RESPONSES_API);

    let slot = LhcCaptureSlot::new();
    assert!(
        slot.buffer_or_handle(PendingCmd::Persist {
            item: user_msg("hi", "u1"),
            provenance: RawItemProvenance::UserPrompt,
        })
        .is_none()
    );
    assert!(
        slot.buffer_or_handle(PendingCmd::Persist {
            item: reasoning("rs_a", "CIPHER_A"),
            provenance: RawItemProvenance::ModelOutput,
        })
        .is_none()
    );
    assert!(
        slot.buffer_or_handle(PendingCmd::SetIdentity {
            identity: id_b.clone(),
        })
        .is_none()
    );
    assert!(
        slot.buffer_or_handle(PendingCmd::Persist {
            item: reasoning("rs_b", "CIPHER_B"),
            provenance: RawItemProvenance::ModelOutput,
        })
        .is_none()
    );

    let handle = open_handle(&root, tid, id_a.clone()).await;
    slot.set_and_flush(handle.clone());
    handle.flush().await;
    assert!(
        handle
            .drain_settled(std::time::Duration::from_secs(120))
            .await
    );
    handle.shutdown().await;

    let ciphers_under = |items: &[codex_protocol::protocol::RolloutItem]| -> Vec<Option<String>> {
        items
            .iter()
            .filter_map(|item| match item {
                codex_protocol::protocol::RolloutItem::ResponseItem(ResponseItem::Reasoning {
                    encrypted_content,
                    ..
                }) => Some(encrypted_content.clone()),
                _ => None,
            })
            .collect()
    };

    let path_a = dir.path().join("ra.jsonl");
    regenerate_rollout_from_thread(
        &path_a,
        tid,
        Some(root.as_path()),
        RolloutReconcileTrigger::Missing,
        Some(id_a),
    )
    .await
    .expect("regen a");
    assert_eq!(
        ciphers_under(&parse_rollout_items(&path_a).unwrap()),
        vec![Some("CIPHER_A".into()), None],
        "identity A must re-emit only the pre-change reasoning"
    );

    let path_b = dir.path().join("rb.jsonl");
    regenerate_rollout_from_thread(
        &path_b,
        tid,
        Some(root.as_path()),
        RolloutReconcileTrigger::Missing,
        Some(id_b),
    )
    .await
    .expect("regen b");
    assert_eq!(
        ciphers_under(&parse_rollout_items(&path_b).unwrap()),
        vec![None, Some("CIPHER_B".into())],
        "identity B must re-emit only the post-change reasoning"
    );
}

/// Pre-open overflow (beyond PRE_OPEN_CAP) latches the capture degraded at
/// handoff — dropped early commands (possibly an identity update) mean the
/// record can no longer be trusted.
#[tokio::test]
async fn pre_open_overflow_degrades_capture() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "pre-open-overflow-tid";
    let slot = LhcCaptureSlot::new();
    for i in 0..=PRE_OPEN_CAP {
        let _ = slot.buffer_or_handle(PendingCmd::Persist {
            item: user_msg("x", &format!("u{i}")),
            provenance: RawItemProvenance::UserPrompt,
        });
    }
    let handle = open_handle(
        &root,
        tid,
        ModelIdentity::new("openai", "gpt-a", ModelIdentity::RESPONSES_API),
    )
    .await;
    slot.set_and_flush(handle.clone());
    assert!(
        handle.is_degraded(),
        "overflowed pre-open buffer must degrade the capture"
    );
    handle.shutdown().await;
}
