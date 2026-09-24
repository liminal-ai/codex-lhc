//! Test-only LHC fold producer for Guardian replay seams.
//!
//! Calls [`materialize_rollout`] with a band + native tool tail so core tests
//! exercise the real Compacted writer (including its stored coverage count)
//! instead of injecting that count by hand.

use crate::CompactBoundaryMeta;
use crate::MaterializeInput;
use crate::materialize_rollout;
use codex_history::GuardianHistoryCheckpoint;
use codex_history::RolloutItem;
use codex_protocol::protocol::SessionMetaLine;
use lhc::intake_stream::TurnOutcome;
use lhc::messages::Block;
use lhc::messages::BlockType;
use lhc::messages::MessageKind;
use lhc::messages::MessageRecord;
use lhc::shared_tech::view::SessionAssistantMessage;
use lhc::shared_tech::view::SessionAssistantPart;
use lhc::shared_tech::view::SessionAssistantPartType;
use lhc::shared_tech::view::SessionThreadView;
use lhc::shared_tech::view::SessionThreadViewEntry;
use lhc::shared_tech::view::SessionThreadViewEntrySource;
use lhc::shared_tech::view::SessionThreadViewMessage;
use lhc::shared_tech::view::SessionToolResultMessage;
use lhc::shared_tech::view::SessionUserMessage;
use lhc::turns::TurnRecord;
use lhc::turns::TurnStatus;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// Extra post-overlap tail the fold producer should emit after the overlapping
/// user/call/result (stale recovery's new B, or a MidTurn unpaired call).
#[derive(Debug, Clone, Copy)]
pub struct GuardianFoldTailExtras<'a> {
    pub extra_call_id: Option<&'a str>,
    pub unpaired_call_id: Option<&'a str>,
}

pub const OVERLAP_CALL_ID: &str = "fc_overlap";
pub const OVERLAP_USER: &str = "read the file";
pub const OVERLAP_OUTPUT: &str = "fn main() {}";

/// Materialize one LHC fold whose tail starts with the overlapping user +
/// `OVERLAP_CALL_ID` tool pair, carrying `guardian` on the Compacted record.
pub fn materialize_guardian_tool_fold(
    guardian: GuardianHistoryCheckpoint,
    extras: GuardianFoldTailExtras<'_>,
) -> Vec<RolloutItem> {
    let mut args = Map::new();
    args.insert("path".into(), json!("src/main.rs"));
    args.insert("__hostRaw".into(), json!("{\"path\":\"src/main.rs\"}"));

    let mut entries = vec![
        band_entry("compressed prior turns"),
        user_tail("m-overlap-user", OVERLAP_USER),
        tool_call_tail("m-overlap-call", OVERLAP_CALL_ID, "read_file", args.clone()),
        tool_result_tail(
            "m-overlap-out",
            OVERLAP_CALL_ID,
            "read_file",
            OVERLAP_OUTPUT,
            Some(false),
        ),
    ];
    let mut messages = vec![
        msg(
            "m-overlap-user",
            "t-overlap",
            MessageKind::UserPrompt,
            10,
            OVERLAP_USER,
        ),
        msg("m-overlap-call", "t-overlap", MessageKind::ToolCall, 11, ""),
        msg_tool_result(
            "m-overlap-out",
            "t-overlap",
            12,
            OVERLAP_CALL_ID,
            OVERLAP_OUTPUT,
            false,
        ),
    ];
    let mut members = vec![
        "m-overlap-user".to_string(),
        "m-overlap-call".to_string(),
        "m-overlap-out".to_string(),
    ];
    let mut order = 13i64;

    if let Some(call_id) = extras.extra_call_id {
        let mid_call = "m-extra-call";
        let mid_out = "m-extra-out";
        entries.push(tool_call_tail(mid_call, call_id, "read_file", args.clone()));
        entries.push(tool_result_tail(
            mid_out,
            call_id,
            "read_file",
            "new-b",
            Some(false),
        ));
        messages.push(msg(mid_call, "t-overlap", MessageKind::ToolCall, order, ""));
        order += 1;
        messages.push(msg_tool_result(
            mid_out,
            "t-overlap",
            order,
            call_id,
            "new-b",
            false,
        ));
        order += 1;
        members.push(mid_call.into());
        members.push(mid_out.into());
    }

    if let Some(call_id) = extras.unpaired_call_id {
        let mid = "m-unpaired";
        entries.push(tool_call_tail(mid, call_id, "read_file", args));
        messages.push(msg(mid, "t-overlap", MessageKind::ToolCall, order, ""));
        members.push(mid.into());
    }

    let view = SessionThreadView {
        thread_id: "guardian-fold".into(),
        entries,
    };
    let turns = [turn("t-overlap", 1, &members)];
    materialize_rollout(&MaterializeInput {
        session_meta: SessionMetaLine {
            meta: Default::default(),
            git: None,
        },
        thread_view: &view,
        messages: &messages,
        turns: &turns,
        events: &[],
        prior_generation: &[],
        prior_realtime_items: &[],
        boundary: CompactBoundaryMeta {
            message: "lhc compact".into(),
            window_number: 1,
            first_window_id: "11111111-1111-7111-8111-111111111111".into(),
            previous_window_id: None,
            window_id: "33333333-3333-7333-8333-333333333333".into(),
        },
        world_state: None,
        turn_context: None,
        guardian_history: Some(guardian),
        retained_context: None,
        latest_token_usage_record: None,
        live_identity: None,
        current_host_turn_id: None,
        current_lhc_turn_id: None,
    })
    .items
}

fn band_entry(text: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
        blocks: None,
        content: format!("[context · brief]\n{text}"),
        source_messages: Vec::new(),
    }))
}

fn user_tail(mid: &str, text: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
        blocks: None,
        content: text.into(),
        source_messages: vec![SessionThreadViewEntrySource {
            message_id: mid.into(),
            idempotency_key: None,
        }],
    }))
}

fn tool_call_tail(
    mid: &str,
    call_id: &str,
    name: &str,
    args: Map<String, Value>,
) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
        SessionAssistantMessage {
            content: vec![SessionAssistantPart {
                block: None,
                type_: SessionAssistantPartType::ToolCall,
                text: None,
                thinking: None,
                thinking_signature: None,
                tool_call_id: Some(call_id.into()),
                tool_name: Some(name.into()),
                arguments: Some(args),
            }],
            source_messages: vec![SessionThreadViewEntrySource {
                message_id: mid.into(),
                idempotency_key: None,
            }],
            provider: None,
            model: None,
            api: None,
        },
    ))
}

fn tool_result_tail(
    mid: &str,
    call_id: &str,
    name: &str,
    content: &str,
    is_error: Option<bool>,
) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(
        SessionToolResultMessage {
            blocks: None,
            tool_call_id: call_id.into(),
            tool_name: Some(name.into()),
            content: content.into(),
            is_error,
            source_messages: vec![SessionThreadViewEntrySource {
                message_id: mid.into(),
                idempotency_key: None,
            }],
        },
    ))
}

fn text_block(text: &str) -> Block {
    let mut content = Map::new();
    content.insert("text".into(), json!(text));
    Block {
        block_type: BlockType::Text,
        content,
    }
}

fn tool_result_block(call_id: &str, content: &str, is_error: bool) -> Block {
    let mut c = Map::new();
    c.insert("toolCallId".into(), json!(call_id));
    c.insert("content".into(), json!(content));
    c.insert("isError".into(), json!(is_error));
    Block {
        block_type: BlockType::ToolResult,
        content: c,
    }
}

fn msg(id: &str, turn_id: &str, kind: MessageKind, order: i64, text: &str) -> MessageRecord {
    let blocks = match kind {
        MessageKind::ToolResult => vec![tool_result_block("x", text, false)],
        MessageKind::ToolCall => {
            let mut c = Map::new();
            c.insert("toolCallId".into(), json!("x"));
            c.insert("toolName".into(), json!("t"));
            c.insert("arguments".into(), json!({}));
            vec![Block {
                block_type: BlockType::ToolCall,
                content: c,
            }]
        }
        _ => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![text_block(text)]
            }
        }
    };
    MessageRecord {
        message_id: id.into(),
        source_event_order: order,
        kind,
        blocks,
        token_estimate: 10,
        actor: "assistant".into(),
        harness: "codex".into(),
        recorded_at: "2026-07-01T12:00:00.000Z".into(),
        turn_id: turn_id.into(),
        provider_usage: None,
        step_index: None,
        derivations: None,
        deleted: None,
    }
}

fn msg_tool_result(
    id: &str,
    turn_id: &str,
    order: i64,
    call_id: &str,
    content: &str,
    is_error: bool,
) -> MessageRecord {
    MessageRecord {
        message_id: id.into(),
        source_event_order: order,
        kind: MessageKind::ToolResult,
        blocks: vec![tool_result_block(call_id, content, is_error)],
        token_estimate: 10,
        actor: "tool".into(),
        harness: "codex".into(),
        recorded_at: "2026-07-01T12:00:00.000Z".into(),
        turn_id: turn_id.into(),
        provider_usage: None,
        step_index: None,
        derivations: None,
        deleted: None,
    }
}

fn turn(id: &str, order: i64, members: &[String]) -> TurnRecord {
    TurnRecord {
        turn_id: id.into(),
        turn_order: order,
        status: TurnStatus::Closed,
        member_message_ids: members.to_vec(),
        opened_at_event_order: order * 10,
        closed_at_event_order: Some(order * 10 + 5),
        outcome: Some(TurnOutcome::Completed),
        outcome_reason: None,
        started_at: Some("2026-07-01T12:00:00.000Z".into()),
        ended_at: Some("2026-07-01T12:00:10.000Z".into()),
        chunk_id: None,
        member_idx: None,
        derivations: None,
    }
}
