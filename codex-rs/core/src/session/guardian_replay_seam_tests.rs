//! Guardian overlap at replay, driven from LHC producers rather than a stored count.

use super::tests::make_session_and_context;
use crate::compact_lhc::drop_materialized_items;
use crate::context::GuardianContextMode;
use codex_history::GuardianHistoryCheckpoint;
use codex_history::RolloutItem;
use codex_lhc_host::BodyValidationSpec;
use codex_lhc_host::GuardianFoldTailExtras;
use codex_lhc_host::OVERLAP_CALL_ID;
use codex_lhc_host::OVERLAP_OUTPUT;
use codex_lhc_host::OVERLAP_USER;
use codex_lhc_host::degrade_body_to_best_available;
use codex_lhc_host::history_from_materialized_items;
use codex_lhc_host::materialize_guardian_tool_fold;
use codex_lhc_host::prior_compact_carry;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn user(text: &str) -> ResponseItem {
    user_with_id(text, None)
}

fn user_with_id(text: &str, id: Option<&str>) -> ResponseItem {
    ResponseItem::Message {
        id: id.map(|id| ResponseItemId::from_server(id.into())),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call(call_id: &str, arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "read_file".into(),
        namespace: None,
        arguments: arguments.into(),
        encrypted_function_args: None,
        call_id: call_id.into(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call_output(call_id: &str, output: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.into()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(output.into()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

fn live_checkpoint() -> Vec<ResponseItem> {
    vec![
        user("Keep the release private."),
        user(OVERLAP_USER),
        function_call(OVERLAP_CALL_ID, "{\"path\":\"src/main.rs\"}"),
        function_call_output(OVERLAP_CALL_ID, OVERLAP_OUTPUT),
    ]
}

fn append_response(items: &mut Vec<RolloutItem>, item: ResponseItem) {
    items.push(RolloutItem::ResponseItem(item.into()));
}

fn guardian_has_call(checkpoint: &GuardianHistoryCheckpoint, call_id: &str) -> bool {
    checkpoint.0.iter().any(|item| match item {
        ResponseItem::FunctionCall { call_id: id, .. } => id == call_id,
        ResponseItem::FunctionCallOutput {
            call_id: Some(id), ..
        } => id == call_id,
        _ => false,
    })
}

fn guardian_has_user(checkpoint: &GuardianHistoryCheckpoint, text: &str) -> bool {
    checkpoint.0.iter().any(|item| match item {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content.iter().any(|part| match part {
                ContentItem::InputText { text: body } => body == text,
                _ => false,
            })
        }
        _ => false,
    })
}

fn producer_covered_count(items: &[RolloutItem]) -> Option<u64> {
    items.iter().rev().find_map(|item| match item {
        RolloutItem::Compacted(compacted) => compacted.guardian_covered_suffix_items,
        _ => None,
    })
}

#[tokio::test]
async fn real_fold_then_new_exchange_is_replayed_on_restart() {
    let (mut session, turn_context) = make_session_and_context().await;
    session.guardian_context_mode = GuardianContextMode::ThreadOwned;
    let mut items = materialize_guardian_tool_fold(
        GuardianHistoryCheckpoint(live_checkpoint()),
        GuardianFoldTailExtras {
            extra_call_id: None,
            unpaired_call_id: None,
            overlap_user_item_id: None,
            user_text_only: false,
        },
    );
    assert_eq!(
        producer_covered_count(&items),
        None,
        "LHC materialize does not write a positional coverage count; replay uses identity"
    );
    let new_instruction = user("Also redact the changelog.");
    let new_call = function_call("fc_post_fold", "{}");
    let new_result = function_call_output("fc_post_fold", "redacted");
    append_response(&mut items, new_instruction.clone());
    append_response(&mut items, new_call);
    append_response(&mut items, new_result);

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &items)
        .await;
    let guardian = reconstructed
        .guardian_history
        .expect("fold snapshot must survive restart");
    assert!(
        guardian_has_user(&guardian, "Keep the release private."),
        "pre-fold evidence must remain: {guardian:?}"
    );
    assert!(
        guardian_has_user(&guardian, "Also redact the changelog."),
        "new post-fold instruction must not be swallowed as overlap: {guardian:?}"
    );
    assert!(
        guardian_has_call(&guardian, "fc_post_fold"),
        "new post-fold tool result must not be swallowed: {guardian:?}"
    );
}

#[tokio::test]
async fn stale_recovery_rematerialize_replays_new_tail_on_restart() {
    let (mut session, turn_context) = make_session_and_context().await;
    session.guardian_context_mode = GuardianContextMode::ThreadOwned;
    let checkpoint_a = GuardianHistoryCheckpoint(live_checkpoint());
    let first = materialize_guardian_tool_fold(
        checkpoint_a.clone(),
        GuardianFoldTailExtras {
            extra_call_id: None,
            unpaired_call_id: None,
            overlap_user_item_id: None,
            user_text_only: false,
        },
    );
    let carry = prior_compact_carry(&first);
    assert_eq!(carry.guardian_history, Some(checkpoint_a.clone()));

    // Interrupted compact / stale recovery: carry old checkpoint A, rebuild a
    // tail that now includes new B. Replay uses identity, not a positional count.
    let recovered = materialize_guardian_tool_fold(
        carry.guardian_history.expect("carried A"),
        GuardianFoldTailExtras {
            extra_call_id: Some("fc_stale_b"),
            unpaired_call_id: None,
            overlap_user_item_id: None,
            user_text_only: false,
        },
    );
    assert_eq!(producer_covered_count(&recovered), None);

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &recovered)
        .await;
    let guardian = reconstructed
        .guardian_history
        .expect("stale recovery snapshot must survive restart");
    assert!(
        guardian_has_call(&guardian, "fc_stale_b"),
        "new B in the rebuilt tail must not be treated as already covered: {guardian:?}"
    );
}

#[tokio::test]
async fn mid_turn_degrade_then_new_response_is_replayed_on_restart() {
    let (mut session, turn_context) = make_session_and_context().await;
    session.guardian_context_mode = GuardianContextMode::ThreadOwned;
    let mut items = materialize_guardian_tool_fold(
        GuardianHistoryCheckpoint(live_checkpoint()),
        GuardianFoldTailExtras {
            extra_call_id: None,
            unpaired_call_id: Some("fc_orphan"),
            overlap_user_item_id: None,
            user_text_only: false,
        },
    );
    assert_eq!(producer_covered_count(&items), None);
    let assembled = history_from_materialized_items(&items);
    let spec = BodyValidationSpec {
        attempt_id: "degrade".into(),
        protected_tool_call_ids: Vec::new(),
        protected_pairs: Vec::new(),
        required_encrypted_reasoning: Vec::new(),
        safe_runway_threshold_tokens: None,
    };
    let degraded = degrade_body_to_best_available(&assembled, &spec);
    assert!(
        degraded.dropped_count() > 0,
        "fixture: unpaired tail call must drop: {}",
        degraded.summary()
    );
    drop_materialized_items(&mut items, &degraded.kept);
    assert_eq!(producer_covered_count(&items), None);

    append_response(&mut items, user("new instruction after degrade"));
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &items)
        .await;
    let guardian = reconstructed
        .guardian_history
        .expect("degraded fold snapshot must survive restart");
    assert!(
        guardian_has_user(&guardian, "new instruction after degrade"),
        "a new item after MidTurn drop must not be swallowed as leftover overlap: {guardian:?}"
    );
}

fn guardian_user_text_count(checkpoint: &GuardianHistoryCheckpoint, text: &str) -> usize {
    checkpoint
        .0
        .iter()
        .filter(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                content.iter().any(|part| match part {
                    ContentItem::InputText { text: body } => body == text,
                    _ => false,
                })
            }
            _ => false,
        })
        .count()
}

#[tokio::test]
async fn real_fold_then_restart_replays_no_duplicate_user_messages() {
    let (mut session, turn_context) = make_session_and_context().await;
    session.guardian_context_mode = GuardianContextMode::ThreadOwned;
    let overlap_id = "msg_overlap_user";
    let items = materialize_guardian_tool_fold(
        GuardianHistoryCheckpoint(vec![
            user("Keep the release private."),
            user_with_id(OVERLAP_USER, Some(overlap_id)),
        ]),
        GuardianFoldTailExtras {
            extra_call_id: None,
            unpaired_call_id: None,
            overlap_user_item_id: Some(overlap_id),
            user_text_only: true,
        },
    );
    let suffix_overlap_ids: Vec<_> = items
        .iter()
        .skip_while(|item| !matches!(item, RolloutItem::Compacted(_)))
        .skip(1)
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::Message {
                    role, id, content, ..
                } if role == "user" => content.iter().find_map(|part| match part {
                    ContentItem::InputText { text } if text == OVERLAP_USER => id.clone(),
                    _ => None,
                }),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        suffix_overlap_ids,
        vec![ResponseItemId::from_server(overlap_id.into())],
        "verbatim user tail must keep the captured host id"
    );

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &items)
        .await;
    let guardian = reconstructed
        .guardian_history
        .expect("user-text fold snapshot must survive restart");
    assert_eq!(
        guardian_user_text_count(&guardian, "Keep the release private."),
        1,
        "pre-fold user must remain once: {guardian:?}"
    );
    assert_eq!(
        guardian_user_text_count(&guardian, OVERLAP_USER),
        1,
        "real fold then restart must not duplicate the overlapping user: {guardian:?}"
    );
}
