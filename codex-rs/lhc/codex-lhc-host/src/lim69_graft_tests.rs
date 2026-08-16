//! LIM-69: grafted CustomToolCall pairs survive rewrite / resume.

use codex_history::CompactedItem;
use codex_history::RolloutItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem as Item;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;

use crate::graft_live_protected_pairs;
use crate::history_from_materialized_items;
use crate::item_bytes_without_id;

#[rustfmt::skip]
fn live_pair() -> Vec<ResponseItem> {
    vec![
        ResponseItem::CustomToolCall {
            id: None, status: Some("completed".into()), call_id: "call-1".into(),
            name: "exec".into(), namespace: None, input: r#"{"cmd":"ls"}"#.into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None, call_id: "call-1".into(), name: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    Item::InputText { text: "part-a".into() },
                    Item::InputText { text: "part-b".into() },
                ]),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

#[test]
fn grafted_custom_tool_pair_survives_rollout_resume() {
    let live = live_pair();
    let mut body = live.clone();
    if let ResponseItem::CustomToolCall { status, .. } = &mut body[0] {
        *status = None;
    }
    if let ResponseItem::CustomToolCallOutput { name, output, .. } = &mut body[1] {
        *name = Some("exec".into());
        output.body = FunctionCallOutputBody::Text("part-a\npart-b".into());
    }
    graft_live_protected_pairs(&mut body, &live, &["call-1".into()]).unwrap();
    for (got, want) in body.iter().zip(&live) {
        assert_eq!(item_bytes_without_id(got), item_bytes_without_id(want));
    }
    #[rustfmt::skip]
    let compacted = CompactedItem {
        message: "lhc".into(), replacement_history: Some(vec![]),
        window_number: None, first_window_id: None, previous_window_id: None, window_id: None,
    };
    let items = vec![
        RolloutItem::Compacted(compacted),
        RolloutItem::ResponseItem(body[0].clone().into()),
        RolloutItem::ResponseItem(body[1].clone().into()),
    ];
    for (got, want) in history_from_materialized_items(&items).iter().zip(&live) {
        assert_eq!(item_bytes_without_id(got), item_bytes_without_id(want));
    }
}
