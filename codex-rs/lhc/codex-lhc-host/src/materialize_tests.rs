//! Golden and invariant tests for the rollout materializer (slice B).

use super::*;
use crate::ModelIdentity;
use codex_protocol::ThreadId;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use lhc::messages::Block;
use lhc::messages::MessageKind;
use lhc::shared_tech::view::SessionAssistantMessage;
use lhc::shared_tech::view::SessionThreadViewEntrySource;
use lhc::shared_tech::view::SessionToolResultMessage;
use lhc::shared_tech::view::SessionUserMessage;
use pretty_assertions::assert_eq;
use serde_json::json;

// ── fixtures ──────────────────────────────────────────────────────────────

fn empty_meta() -> SessionMetaLine {
    SessionMetaLine {
        meta: Default::default(),
        git: None,
    }
}

fn boundary(n: u64) -> CompactBoundaryMeta {
    CompactBoundaryMeta {
        message: "lhc compact".into(),
        window_number: n,
        first_window_id: "11111111-1111-7111-8111-111111111111".into(),
        previous_window_id: if n > 1 {
            Some("22222222-2222-7222-8222-222222222222".into())
        } else {
            None
        },
        window_id: "33333333-3333-7333-8333-333333333333".into(),
    }
}

fn band_entry(text: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
        content: format!("[context · brief]\n{text}"),
        source_messages: Vec::new(),
    }))
}

fn user_tail(mid: &str, text: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
        content: text.into(),
        source_messages: vec![SessionThreadViewEntrySource {
            message_id: mid.into(),
            idempotency_key: None,
        }],
    }))
}

fn assistant_text_tail(mid: &str, text: &str, key: Option<&str>) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
        SessionAssistantMessage {
            content: vec![SessionAssistantPart {
                type_: SessionAssistantPartType::Text,
                text: Some(text.into()),
                thinking: None,
                thinking_signature: None,
                tool_call_id: None,
                tool_name: None,
                arguments: None,
            }],
            source_messages: vec![SessionThreadViewEntrySource {
                message_id: mid.into(),
                idempotency_key: key.map(str::to_string),
            }],
            provider: None,
            model: None,
            api: None,
        },
    ))
}

fn thinking_tail(mid: &str, text: &str) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
        SessionAssistantMessage {
            content: vec![SessionAssistantPart {
                type_: SessionAssistantPartType::Thinking,
                text: None,
                thinking: Some(text.into()),
                thinking_signature: None,
                tool_call_id: None,
                tool_name: None,
                arguments: None,
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

fn tool_call_tail(
    mid: &str,
    call_id: &str,
    name: &str,
    args: Map<String, Value>,
) -> SessionThreadViewEntry {
    tool_call_tail_with_key(mid, call_id, name, args, None)
}

fn tool_call_tail_with_key(
    mid: &str,
    call_id: &str,
    name: &str,
    args: Map<String, Value>,
    key: Option<&str>,
) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
        SessionAssistantMessage {
            content: vec![SessionAssistantPart {
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
                idempotency_key: key.map(str::to_string),
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
    tool_result_tail_with_key(mid, call_id, name, content, is_error, None)
}

fn tool_result_tail_with_key(
    mid: &str,
    call_id: &str,
    name: &str,
    content: &str,
    is_error: Option<bool>,
    key: Option<&str>,
) -> SessionThreadViewEntry {
    SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(
        SessionToolResultMessage {
            tool_call_id: call_id.into(),
            tool_name: Some(name.into()),
            content: content.into(),
            is_error,
            source_messages: vec![SessionThreadViewEntrySource {
                message_id: mid.into(),
                idempotency_key: key.map(str::to_string),
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

fn msg(
    id: &str,
    turn_id: &str,
    kind: MessageKind,
    order: i64,
    text: &str,
    provider_usage: Option<Map<String, Value>>,
) -> MessageRecord {
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
        provider_usage,
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
        derivations: None,
        deleted: None,
    }
}

fn turn(
    id: &str,
    order: i64,
    members: &[&str],
    outcome: Option<TurnOutcome>,
    reason: Option<&str>,
    started: Option<&str>,
    ended: Option<&str>,
) -> TurnRecord {
    turn_with_status(
        id,
        order,
        members,
        TurnStatus::Closed,
        outcome,
        reason,
        started,
        ended,
    )
}

fn turn_with_status(
    id: &str,
    order: i64,
    members: &[&str],
    status: TurnStatus,
    outcome: Option<TurnOutcome>,
    reason: Option<&str>,
    started: Option<&str>,
    ended: Option<&str>,
) -> TurnRecord {
    TurnRecord {
        turn_id: id.into(),
        turn_order: order,
        status,
        member_message_ids: members.iter().map(|s| (*s).to_string()).collect(),
        opened_at_event_order: order * 10,
        closed_at_event_order: if status == TurnStatus::Closed {
            Some(order * 10 + 5)
        } else {
            None
        },
        outcome,
        outcome_reason: reason.map(str::to_string),
        started_at: started.map(str::to_string),
        ended_at: ended.map(str::to_string),
        chunk_id: None,
        member_idx: None,
        derivations: None,
    }
}

fn materialize(
    view: SessionThreadView,
    messages: &[MessageRecord],
    turns: &[TurnRecord],
    prior: &[RolloutItem],
    world: Option<Value>,
) -> Vec<RolloutItem> {
    materialize_full(view, messages, turns, prior, world, None).items
}

fn materialize_full(
    view: SessionThreadView,
    messages: &[MessageRecord],
    turns: &[TurnRecord],
    prior: &[RolloutItem],
    world: Option<Value>,
    turn_context: Option<TurnContextItem>,
) -> MaterializeResult {
    materialize_rollout(&MaterializeInput {
        session_meta: empty_meta(),
        thread_view: &view,
        messages,
        turns,
        prior_generation: prior,
        boundary: boundary(1),
        world_state: world,
        turn_context,
        live_identity: None,
    })
}

fn materialize_with_ctx(
    view: SessionThreadView,
    messages: &[MessageRecord],
    turns: &[TurnRecord],
    prior: &[RolloutItem],
    world: Option<Value>,
    turn_context: Option<TurnContextItem>,
) -> Vec<RolloutItem> {
    materialize_full(view, messages, turns, prior, world, turn_context).items
}

fn find_compacted(items: &[RolloutItem]) -> &CompactedItem {
    items
        .iter()
        .find_map(|i| match i {
            RolloutItem::Compacted(c) => Some(c),
            _ => None,
        })
        .expect("Compacted present")
}

fn tail_response_items(items: &[RolloutItem]) -> Vec<&ResponseItem> {
    let mut past = false;
    let mut out = Vec::new();
    for item in items {
        match item {
            RolloutItem::Compacted(_) => past = true,
            RolloutItem::ResponseItem(r) if past => out.push(r),
            _ => {}
        }
    }
    out
}

fn usage(total: i64) -> Map<String, Value> {
    json!({
        "input_tokens": total - 10,
        "cached_input_tokens": 0,
        "cache_write_input_tokens": 0,
        "output_tokens": 10,
        "reasoning_output_tokens": 0,
        "total_tokens": total
    })
    .as_object()
    .cloned()
    .unwrap()
}

// ── structure / boundary ──────────────────────────────────────────────────

#[test]
fn structure_session_meta_then_stream_then_one_compacted_then_world_then_tail() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("old history summary"),
            user_tail("m1", "post-boundary prompt"),
            assistant_text_tail("m2", "ok", None),
        ],
    };
    let messages = [
        msg(
            "m0",
            "t0",
            MessageKind::UserPrompt,
            1,
            "very first prompt",
            None,
        ),
        msg(
            "m1",
            "turn-1",
            MessageKind::UserPrompt,
            10,
            "post-boundary prompt",
            None,
        ),
        msg("m2", "turn-1", MessageKind::AssistantText, 11, "ok", None),
    ];
    let turns = [turn(
        "turn-1",
        1,
        &["m1", "m2"],
        Some(TurnOutcome::Completed),
        None,
        Some("2026-07-01T12:00:00.000Z"),
        Some("2026-07-01T12:00:04.000Z"),
    )];
    let items = materialize(view, &messages, &turns, &[], Some(json!({"cwd": "/tmp"})));

    assert!(matches!(items.first(), Some(RolloutItem::SessionMeta(_))));
    assert_eq!(boundary_completeness_error(&items), None);
    assert_eq!(
        items
            .iter()
            .filter(|i| matches!(i, RolloutItem::Compacted(_)))
            .count(),
        1
    );
    let tail = tail_response_items(&items);
    assert!(tail.iter().any(|r| matches!(
        r,
        ResponseItem::Message { role, content, .. }
            if role == "user" && content_text(content).contains("post-boundary")
    )));
}

#[test]
fn boundary_record_field_completeness_pinned() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![band_entry("band")],
    };
    let items = materialize(view, &[], &[], &[], None);
    let c = find_compacted(&items);
    assert!(
        c.replacement_history
            .as_ref()
            .is_some_and(|h| !h.is_empty())
    );
    assert_eq!(c.window_number, Some(1));
    assert_eq!(boundary_completeness_error(&items), None);

    // Empty replacement_history is rejected (M13).
    assert_eq!(
        boundary_completeness_error(&[RolloutItem::Compacted(CompactedItem {
            message: "x".into(),
            replacement_history: Some(Vec::new()),
            window_number: Some(1),
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        })]),
        Some("Compacted.replacement_history is empty")
    );
    assert_eq!(
        boundary_completeness_error(&[RolloutItem::Compacted(CompactedItem {
            message: "x".into(),
            replacement_history: None,
            window_number: Some(1),
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        })]),
        Some("Compacted.replacement_history is None")
    );
    assert_eq!(
        boundary_completeness_error(&[RolloutItem::Compacted(CompactedItem {
            message: "x".into(),
            replacement_history: Some(vec![user_text_message("x")]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        })]),
        Some("Compacted.window_number is None")
    );
}

// ── C1: rollback applied by exclusion ─────────────────────────────────────

#[test]
fn c1_rollback_excludes_dropped_turns_keeps_live_tail_no_marker() {
    // Prior gen (chronological):
    //   TurnStarted t-rb1, UserMessage "rolled-A", TurnComplete,
    //   TurnStarted t-rb2, UserMessage "rolled-B", TurnComplete,
    //   ThreadRolledBack{2},
    //   TurnStarted t-live1, UserMessage "live-1", TurnComplete,
    //   TurnStarted t-live2, UserMessage "live-2", TurnComplete
    // Reverse-scan drops rolled-B then rolled-A (the 2 before the marker).
    let prior = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t-rb1".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("rolled-A"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t-rb1".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t-rb2".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("rolled-B"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t-rb2".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 2,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t-live1".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("live-1"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t-live1".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t-live2".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("live-2"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t-live2".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    ];

    // LHC still holds all four turns (rollback never captured).
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("compressed older"),
            user_tail("m-rb1", "rolled-A"),
            assistant_text_tail("m-rb1a", "a", None),
            user_tail("m-rb2", "rolled-B"),
            assistant_text_tail("m-rb2a", "b", None),
            user_tail("m-l1", "live-1"),
            assistant_text_tail("m-l1a", "ok1", None),
            user_tail("m-l2", "live-2"),
            assistant_text_tail("m-l2a", "ok2", None),
        ],
    };
    let messages = [
        msg(
            "m-rb1",
            "t-rb1",
            MessageKind::UserPrompt,
            10,
            "rolled-A",
            None,
        ),
        msg("m-rb1a", "t-rb1", MessageKind::AssistantText, 11, "a", None),
        msg(
            "m-rb2",
            "t-rb2",
            MessageKind::UserPrompt,
            20,
            "rolled-B",
            None,
        ),
        msg("m-rb2a", "t-rb2", MessageKind::AssistantText, 21, "b", None),
        msg(
            "m-l1",
            "t-live1",
            MessageKind::UserPrompt,
            30,
            "live-1",
            None,
        ),
        msg(
            "m-l1a",
            "t-live1",
            MessageKind::AssistantText,
            31,
            "ok1",
            None,
        ),
        msg(
            "m-l2",
            "t-live2",
            MessageKind::UserPrompt,
            40,
            "live-2",
            None,
        ),
        msg(
            "m-l2a",
            "t-live2",
            MessageKind::AssistantText,
            41,
            "ok2",
            None,
        ),
    ];
    let turns = [
        turn(
            "t-rb1",
            1,
            &["m-rb1", "m-rb1a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t-rb2",
            2,
            &["m-rb2", "m-rb2a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t-live1",
            3,
            &["m-l1", "m-l1a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t-live2",
            4,
            &["m-l2", "m-l2a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
    ];

    let items = materialize(view, &messages, &turns, &prior, None);

    // No ThreadRolledBack in the rebuilt file.
    assert!(
        !items
            .iter()
            .any(|i| matches!(i, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)))),
        "rollback marker must not be carried"
    );

    let tail = tail_response_items(&items);
    let user_texts: Vec<String> = tail
        .iter()
        .filter_map(|r| match r {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                Some(content_text(content))
            }
            _ => None,
        })
        .collect();
    assert!(
        user_texts.iter().any(|t| t == "live-1"),
        "live turns must be present: {user_texts:?}"
    );
    assert!(user_texts.iter().any(|t| t == "live-2"));
    assert!(
        !user_texts
            .iter()
            .any(|t| t == "rolled-A" || t == "rolled-B"),
        "rolled-back turns must be absent from tail: {user_texts:?}"
    );
}

/// Duplicate prompt text: dropped "yes" then live "yes" - live must survive.
#[test]
fn c1_duplicate_text_excludes_only_positionally_dropped_turn() {
    // Prior: t-drop "yes", RolledBack{1}, t-live "yes".
    // Reverse-scan drops only the first; text-set matching would kill both.
    let prior = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t-drop".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("yes"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t-drop".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t-live".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("yes"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t-live".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    ];

    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("older"),
            user_tail("m-drop", "yes"),
            assistant_text_tail("m-drop-a", "ack1", None),
            user_tail("m-live", "yes"),
            assistant_text_tail("m-live-a", "ack2", None),
        ],
    };
    let messages = [
        msg("m-drop", "t-drop", MessageKind::UserPrompt, 10, "yes", None),
        msg(
            "m-drop-a",
            "t-drop",
            MessageKind::AssistantText,
            11,
            "ack1",
            None,
        ),
        msg("m-live", "t-live", MessageKind::UserPrompt, 20, "yes", None),
        msg(
            "m-live-a",
            "t-live",
            MessageKind::AssistantText,
            21,
            "ack2",
            None,
        ),
    ];
    let turns = [
        turn(
            "t-drop",
            1,
            &["m-drop", "m-drop-a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t-live",
            2,
            &["m-live", "m-live-a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
    ];

    let items = materialize(view, &messages, &turns, &prior, None);
    let tail = tail_response_items(&items);
    let user_texts: Vec<String> = tail
        .iter()
        .filter_map(|r| match r {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                Some(content_text(content))
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        user_texts.iter().filter(|t| *t == "yes").count(),
        1,
        "exactly one live 'yes' must remain: {user_texts:?}"
    );
    assert!(
        tail.iter().any(|r| matches!(
            r,
            ResponseItem::Message { role, content, .. }
                if role == "assistant" && content_text(content) == "ack2"
        )),
        "live turn assistant must survive: {tail:?}"
    );
    assert!(
        !tail.iter().any(|r| matches!(
            r,
            ResponseItem::Message { role, content, .. }
                if role == "assistant" && content_text(content) == "ack1"
        )),
        "dropped turn assistant must be absent"
    );
}

/// Alignment mismatch -> exclude nothing + loud gap note.
#[test]
fn c1_alignment_mismatch_excludes_nothing_and_logs_gap() {
    let prior = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t1".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("alpha"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t1".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t2".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("live-ok"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t2".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    ];

    // LHC diverged at position 0: "beta" vs prior "alpha".
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "beta"),
            assistant_text_tail("m1a", "x", None),
            user_tail("m2", "live-ok"),
            assistant_text_tail("m2a", "y", None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 10, "beta", None),
        msg("m1a", "t1", MessageKind::AssistantText, 11, "x", None),
        msg("m2", "t2", MessageKind::UserPrompt, 20, "live-ok", None),
        msg("m2a", "t2", MessageKind::AssistantText, 21, "y", None),
    ];
    let turns = [
        turn(
            "t1",
            1,
            &["m1", "m1a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t2",
            2,
            &["m2", "m2a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
    ];

    let result = materialize_full(view, &messages, &turns, &prior, None, None);
    assert!(
        !result.gap_notes.is_empty(),
        "alignment mismatch must produce a gap note"
    );
    assert!(
        result
            .gap_notes
            .iter()
            .any(|n| n.contains("alignment mismatch") && n.contains("under-exclusion")),
        "gap note must name the mismatch: {:?}",
        result.gap_notes
    );

    let tail = tail_response_items(&result.items);
    let user_texts: Vec<String> = tail
        .iter()
        .filter_map(|r| match r {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                Some(content_text(content))
            }
            _ => None,
        })
        .collect();
    assert!(
        user_texts.iter().any(|t| t == "beta"),
        "mismatch must not exclude: {user_texts:?}"
    );
    assert!(user_texts.iter().any(|t| t == "live-ok"));
}

// ── H2: cumulative token totals ───────────────────────────────────────────

#[test]
fn h2_token_count_total_is_cumulative_across_calls() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "hi"),
            assistant_text_tail("m2", "one", None),
            assistant_text_tail("m3", "two", None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "hi", None),
        msg(
            "m2",
            "t1",
            MessageKind::AssistantText,
            2,
            "one",
            Some(usage(100)),
        ),
        msg(
            "m3",
            "t1",
            MessageKind::AssistantText,
            3,
            "two",
            Some(usage(50)),
        ),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);

    let token_counts: Vec<&TokenUsageInfo> = items
        .iter()
        .filter_map(|i| match i {
            RolloutItem::EventMsg(EventMsg::TokenCount(e)) => e.info.as_ref(),
            _ => None,
        })
        .collect();
    assert_eq!(token_counts.len(), 2);
    // First call: total=last=100
    assert_eq!(token_counts[0].last_token_usage.total_tokens, 100);
    assert_eq!(token_counts[0].total_token_usage.total_tokens, 100);
    // Second call: last=50, total=150
    assert_eq!(token_counts[1].last_token_usage.total_tokens, 50);
    assert_eq!(token_counts[1].total_token_usage.total_tokens, 150);

    // Newest TokenCount (what consumers read) carries the session total.
    let newest = token_counts.last().unwrap();
    assert_eq!(newest.total_token_usage.total_tokens, 150);
}

/// H2: TokenCount emits after the last part of a usage-bearing message even
/// when that part is a tool call (not text).
#[test]
fn h2_token_count_emits_after_last_part_regardless_of_kind() {
    let mut args = Map::new();
    args.insert("path".into(), json!("x"));
    args.insert("__hostRaw".into(), json!("{\"path\":\"x\"}"));
    // One assistant message grouped as thinking + toolCall (two parts, one msg).
    // provider_usage rides the message; emit after the toolCall part.
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "go"),
            SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
                SessionAssistantMessage {
                    content: vec![
                        SessionAssistantPart {
                            type_: SessionAssistantPartType::Thinking,
                            text: None,
                            thinking: Some("plan".into()),
                            thinking_signature: None,
                            tool_call_id: None,
                            tool_name: None,
                            arguments: None,
                        },
                        SessionAssistantPart {
                            type_: SessionAssistantPartType::ToolCall,
                            text: None,
                            thinking: None,
                            thinking_signature: None,
                            tool_call_id: Some("fc_u".into()),
                            tool_name: Some("read_file".into()),
                            arguments: Some(args),
                        },
                    ],
                    // Both parts share the same source message (one LHC row with usage).
                    source_messages: vec![
                        SessionThreadViewEntrySource {
                            message_id: "m2".into(),
                            idempotency_key: None,
                        },
                        SessionThreadViewEntrySource {
                            message_id: "m2".into(),
                            idempotency_key: None,
                        },
                    ],
                    provider: None,
                    model: None,
                    api: None,
                },
            )),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "go", None),
        // Single assistant_thinking-shaped row carrying usage (tool-heavy msg).
        MessageRecord {
            message_id: "m2".into(),
            source_event_order: 2,
            kind: MessageKind::AssistantThinking,
            blocks: vec![text_block("plan")],
            token_estimate: 10,
            actor: "assistant".into(),
            harness: "codex".into(),
            recorded_at: "2026-07-01T12:00:00.000Z".into(),
            turn_id: "t1".into(),
            provider_usage: Some(usage(77)),
            derivations: None,
            deleted: None,
        },
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);

    // Find positions: last ResponseItem for m2's parts, then TokenCount.
    let mut last_part_idx = None;
    let mut token_idx = None;
    for (i, item) in items.iter().enumerate() {
        match item {
            RolloutItem::ResponseItem(ResponseItem::FunctionCall { call_id, .. })
                if call_id == "fc_u" =>
            {
                last_part_idx = Some(i);
            }
            RolloutItem::EventMsg(EventMsg::TokenCount(e))
                if e.info
                    .as_ref()
                    .is_some_and(|info| info.last_token_usage.total_tokens == 77) =>
            {
                token_idx = Some(i);
            }
            _ => {}
        }
    }
    assert!(last_part_idx.is_some(), "tool call part must be present");
    assert!(
        token_idx.is_some(),
        "TokenCount for usage-bearing msg required"
    );
    assert!(
        token_idx.unwrap() > last_part_idx.unwrap(),
        "TokenCount must follow the last part of the usage-bearing message"
    );
}

/// H2: rolled-back turns' provider_usage must not inflate cumulative totals.
#[test]
fn h2_cumulative_excludes_rollback_excluded_turns() {
    let prior = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t-rb".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("gone"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t-rb".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "t-live".into(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        user_message_event("live"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t-live".into(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    ];
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m-rb", "gone"),
            assistant_text_tail("m-rb-a", "x", None),
            user_tail("m-live", "live"),
            assistant_text_tail("m-live-a", "y", None),
        ],
    };
    let messages = [
        msg("m-rb", "t-rb", MessageKind::UserPrompt, 10, "gone", None),
        // Rolled-back turn carried 1000 tokens - must not inflate live total.
        msg(
            "m-rb-a",
            "t-rb",
            MessageKind::AssistantText,
            11,
            "x",
            Some(usage(1000)),
        ),
        msg(
            "m-live",
            "t-live",
            MessageKind::UserPrompt,
            20,
            "live",
            None,
        ),
        msg(
            "m-live-a",
            "t-live",
            MessageKind::AssistantText,
            21,
            "y",
            Some(usage(50)),
        ),
    ];
    let turns = [
        turn(
            "t-rb",
            1,
            &["m-rb", "m-rb-a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t-live",
            2,
            &["m-live", "m-live-a"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
    ];
    let items = materialize(view, &messages, &turns, &prior, None);
    let totals: Vec<i64> = items
        .iter()
        .filter_map(|i| match i {
            RolloutItem::EventMsg(EventMsg::TokenCount(e)) => e
                .info
                .as_ref()
                .map(|info| info.total_token_usage.total_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(
        totals,
        vec![50],
        "only live usage; rolled-back 1000 excluded"
    );
}

// ── H3: image-gen one item when paired ────────────────────────────────────

#[test]
fn h3_image_generation_paired_emits_one_item_unpaired_degraded() {
    let mut args = Map::new();
    args.insert("revisedPrompt".into(), json!("a cat"));

    // Paired case.
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "draw"),
            tool_call_tail("m2", "ig_1", "image_generation", args.clone()),
            tool_result_tail(
                "m3",
                "ig_1",
                "image_generation",
                r#"{"status":"completed","result":"base64…","revisedPrompt":"a cat"}"#,
                None,
            ),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "draw", None),
        msg("m2", "t1", MessageKind::ToolCall, 2, "", None),
        msg_tool_result(
            "m3",
            "t1",
            3,
            "ig_1",
            r#"{"status":"completed","result":"base64…","revisedPrompt":"a cat"}"#,
            false,
        ),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);
    let tail = tail_response_items(&items);
    let ig_count = tail
        .iter()
        .filter(|r| matches!(r, ResponseItem::ImageGenerationCall { .. }))
        .count();
    assert_eq!(ig_count, 1, "paired -> exactly one ImageGenerationCall");
    assert!(tail.iter().any(|r| matches!(
        r,
        ResponseItem::ImageGenerationCall { status, result, .. }
            if status == "completed" && result == "base64…"
    )));
    let end_count = items
        .iter()
        .filter(|i| matches!(i, RolloutItem::EventMsg(EventMsg::ImageGenerationEnd(_))))
        .count();
    assert_eq!(end_count, 1);

    // Unpaired case: degraded ImageGenerationCall must land BEFORE TurnComplete.
    let view2 = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "draw"),
            tool_call_tail("m2", "ig_2", "image_generation", args),
        ],
    };
    let messages2 = [
        msg("m1", "t2", MessageKind::UserPrompt, 1, "draw", None),
        msg("m2", "t2", MessageKind::ToolCall, 2, "", None),
    ];
    let turns2 = [turn(
        "t2",
        1,
        &["m1", "m2"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items2 = materialize(view2, &messages2, &turns2, &[], None);
    let mut saw_degraded_ig = false;
    let mut saw_complete_after_ig = false;
    for i in &items2 {
        match i {
            RolloutItem::ResponseItem(ResponseItem::ImageGenerationCall {
                status, result, ..
            }) if status == "unknown" && result.is_empty() => {
                saw_degraded_ig = true;
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(e))
                if e.turn_id == "t2" && saw_degraded_ig =>
            {
                saw_complete_after_ig = true;
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(e))
                if e.turn_id == "t2" && !saw_degraded_ig =>
            {
                panic!("TurnComplete before unpaired ImageGenerationCall flush");
            }
            _ => {}
        }
    }
    assert!(
        saw_degraded_ig,
        "unpaired degraded ImageGenerationCall required"
    );
    assert!(
        saw_complete_after_ig,
        "unpaired ImageGenerationCall must precede its turn's TurnComplete"
    );
}

#[test]
fn function_call_preserves_present_empty_encrypted_args() {
    let mut args = Map::new();
    args.insert("__hostRaw".into(), json!("{}"));
    args.insert("__hostEncryptedFunctionArgs".into(), json!([]));
    let (item, kind) = reverse_tool_call("search", "call-1", &args, Some("fc_1"), &mut Vec::new());

    assert_eq!(kind, RecoveredToolCallKind::Function);
    assert!(matches!(
        item,
        ResponseItem::FunctionCall {
            encrypted_function_args: Some(values),
            ..
        } if values.is_empty()
    ));
}

/// Law 6: tool result routing by name only - content sniffer must not misroute.
#[test]
fn law6_function_result_with_image_shaped_body_stays_function_output() {
    let mut args = Map::new();
    args.insert("q".into(), json!("x"));
    args.insert("__hostRaw".into(), json!("{\"q\":\"x\"}"));
    // Body looks like an image_generation tool_result payload - must NOT sniffer-route.
    let body = r#"{"status":"completed","result":"not-an-image","revisedPrompt":"trap"}"#;

    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "call"),
            tool_call_tail("m2", "fc_trap", "search", args),
            tool_result_tail("m3", "fc_trap", "search", body, None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "call", None),
        msg("m2", "t1", MessageKind::ToolCall, 2, "", None),
        msg_tool_result("m3", "t1", 3, "fc_trap", body, false),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);
    let tail = tail_response_items(&items);
    assert!(
        tail.iter().any(|r| matches!(
            r,
            ResponseItem::FunctionCall { call_id, name, .. }
                if call_id == "fc_trap" && name == "search"
        )),
        "FunctionCall must remain: {tail:?}"
    );
    assert!(
        tail.iter().any(|r| matches!(
            r,
            ResponseItem::FunctionCallOutput { call_id, output, .. }
                if call_id == "fc_trap"
                    && matches!(&output.body, FunctionCallOutputBody::Text(t) if t == body)
        )),
        "must stay FunctionCallOutput, not ImageGenerationCall: {tail:?}"
    );
    assert!(
        !tail
            .iter()
            .any(|r| matches!(r, ResponseItem::ImageGenerationCall { .. })),
        "content sniffer must not invent ImageGenerationCall"
    );
}

// ── F-L2: ctc_ / ctco_ id prefix -> CustomToolCall kind (never FunctionCall) ─

#[test]
fn fl2_ctc_id_round_trips_as_custom_tool_call_with_id() {
    // Host id `ctc_…` recovered from id-primary idempotency key must reverse
    // as CustomToolCall (input from stored arguments), not FunctionCall.
    let call_id = "call_WzQWJPGLgp4UwPLYaxiCYvGu";
    let host_id = "ctc_0695851a6456c30b016a66a651de78819c887458ac0e8029a7";
    let out_id = "ctco_019fa0f9-ba40-7780-b860-d52dbbf93f85";
    let key = format!("codex:tid123:id:{host_id}:deadbeef:tool_call:{call_id}");
    let out_key = format!("codex:tid123:id:{out_id}:cafebabe:tool_result:{call_id}");
    let mut args = Map::new();
    args.insert(
        "__hostRaw".into(),
        json!("const r = await tools.exec_command({cmd:\"ls\"});"),
    );
    args.insert("cmd".into(), json!("ls"));
    let body = "numbers.csv\nreadme.txt\n";
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            user_tail("m1", "list files"),
            tool_call_tail_with_key("m2", call_id, "exec", args, Some(&key)),
            tool_result_tail_with_key("m3", call_id, "exec", body, Some(false), Some(&out_key)),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "list files", None),
        msg("m2", "t1", MessageKind::ToolCall, 2, "", None),
        msg_tool_result("m3", "t1", 3, call_id, body, false),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let result = materialize_full(view, &messages, &turns, &[], None, None);
    let tail = tail_response_items(&result.items);
    let call = tail.iter().find_map(|r| match r {
        ResponseItem::CustomToolCall {
            id,
            call_id: cid,
            name,
            input,
            ..
        } => Some((id.clone(), cid.clone(), name.clone(), input.clone())),
        _ => None,
    });
    let (id, cid, name, input) =
        call.unwrap_or_else(|| panic!("CustomToolCall required, got {tail:?}"));
    assert_eq!(cid, call_id);
    assert_eq!(name, "exec");
    assert!(
        input.contains("exec_command") || input.contains("ls"),
        "input from stored payload: {input}"
    );
    assert_eq!(
        id.as_ref().map(codex_protocol::ResponseItemId::as_str),
        Some(host_id),
        "ctc_ id must be retained on CustomToolCall"
    );
    assert!(
        !tail
            .iter()
            .any(|r| matches!(r, ResponseItem::FunctionCall { .. })),
        "must not emit FunctionCall wearing a ctc_ id: {tail:?}"
    );
    let out = tail.iter().find_map(|r| match r {
        ResponseItem::CustomToolCallOutput {
            id,
            call_id: cid,
            output,
            ..
        } => Some((id.clone(), cid.clone(), output.clone())),
        _ => None,
    });
    let (oid, ocid, output) =
        out.unwrap_or_else(|| panic!("CustomToolCallOutput required, got {tail:?}"));
    assert_eq!(ocid, call_id);
    assert_eq!(
        oid.as_ref().map(codex_protocol::ResponseItemId::as_str),
        Some(out_id)
    );
    assert!(matches!(
        &output.body,
        FunctionCallOutputBody::Text(t) if t == body
    ));
}

#[test]
fn fl2_unrepresentable_id_prefix_clears_id_with_gap() {
    // msg_ prefix on a tool_call is unrepresentable as FunctionCall/Custom -
    // reverse must clear id (provider remints) and record a gap note.
    let call_id = "call_bad";
    let host_id = "msg_should_not_be_on_tool_call";
    let key = format!("codex:tid123:id:{host_id}:deadbeef:tool_call:{call_id}");
    let mut args = Map::new();
    args.insert("__hostRaw".into(), json!("{}"));
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            user_tail("m1", "x"),
            tool_call_tail_with_key("m2", call_id, "exec", args, Some(&key)),
            tool_result_tail("m3", call_id, "exec", "ok", Some(false)),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "x", None),
        msg("m2", "t1", MessageKind::ToolCall, 2, "", None),
        msg_tool_result("m3", "t1", 3, call_id, "ok", false),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let result = materialize_full(view, &messages, &turns, &[], None, None);
    let tail = tail_response_items(&result.items);
    let call = tail.iter().find(|r| {
        matches!(
            r,
            ResponseItem::FunctionCall { .. } | ResponseItem::CustomToolCall { .. }
        )
    });
    let call = call.expect("tool call present");
    assert!(
        call.id().is_none(),
        "unrepresentable id must be cleared: {call:?}"
    );
    assert!(
        result
            .gap_notes
            .iter()
            .any(|n| n.contains("unrepresentable")),
        "gap_notes must record unrepresentable id: {:?}",
        result.gap_notes
    );
}

#[test]
fn fl2_mutation_demo_ctc_must_not_become_function_call() {
    // Mutation target: if reverse always emitted FunctionCall, this would
    // pair a ctc_ id with FunctionCall - the live-cert 400 class of bug.
    let call_id = "call_mut";
    let host_id = "ctc_mutation_probe_id";
    let key = format!("codex:t:id:{host_id}:d:tool_call:{call_id}");
    let mut args = Map::new();
    args.insert("__hostRaw".into(), json!("input-body"));
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![tool_call_tail_with_key(
            "m1",
            call_id,
            "shell",
            args,
            Some(&key),
        )],
    };
    let messages = [msg("m1", "t1", MessageKind::ToolCall, 1, "", None)];
    let turns = [turn(
        "t1",
        1,
        &["m1"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);
    let tail = tail_response_items(&items);
    for r in &tail {
        if let ResponseItem::FunctionCall { id: Some(id), .. } = r {
            assert!(
                !id.as_str().starts_with("ctc"),
                "MUTATION: FunctionCall must never wear a ctc_ id (got {id})"
            );
        }
    }
    assert!(
        tail.iter().any(|r| matches!(
            r,
            ResponseItem::CustomToolCall { id: Some(id), .. }
                if id.as_str() == host_id
        )),
        "ctc_ must reverse as CustomToolCall: {tail:?}"
    );
}

// ── H4: open turns stay open; close-before-open ordering ──────────────────

#[test]
fn h4_open_turn_emits_no_turn_complete() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "hello"),
            assistant_text_tail("m2", "partial", None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "hello", None),
        msg("m2", "t1", MessageKind::AssistantText, 2, "partial", None),
    ];
    let turns = [turn_with_status(
        "t1",
        1,
        &["m1", "m2"],
        TurnStatus::Open,
        None,
        None,
        Some("2026-07-01T12:00:00.000Z"),
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);
    assert!(items.iter().any(|i| matches!(
        i,
        RolloutItem::EventMsg(EventMsg::TurnStarted(e)) if e.turn_id == "t1"
    )));
    assert!(
        !items.iter().any(|i| matches!(
            i,
            RolloutItem::EventMsg(EventMsg::TurnComplete(_))
                | RolloutItem::EventMsg(EventMsg::TurnAborted(_))
        )),
        "open turn must not get a fabricated end event"
    );
}

/// Closed turn whose last member is missing from the view closes before the
/// next turn's TurnStarted (not in an end-of-file sweep after it).
#[test]
fn h4_closed_turn_with_memberless_gap_closes_before_next_started() {
    // t1 members: m1, m2 - but view only has m1 (user). m2 never appears ->
    // maybe_close_turn_if_last never fires. Opening t2 must close t1 first.
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "turn one"),
            // m2 (assistant) deliberately absent from the view
            user_tail("m3", "turn two"),
            assistant_text_tail("m4", "ok", None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "turn one", None),
        msg(
            "m2",
            "t1",
            MessageKind::AssistantText,
            2,
            "missing from view",
            None,
        ),
        msg("m3", "t2", MessageKind::UserPrompt, 3, "turn two", None),
        msg("m4", "t2", MessageKind::AssistantText, 4, "ok", None),
    ];
    let turns = [
        turn(
            "t1",
            1,
            &["m1", "m2"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t2",
            2,
            &["m3", "m4"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
    ];
    let items = materialize(view, &messages, &turns, &[], None);

    let mut saw_t1_complete = false;
    let mut t2_started_after_t1_complete = false;
    for i in &items {
        match i {
            RolloutItem::EventMsg(EventMsg::TurnComplete(e)) if e.turn_id == "t1" => {
                saw_t1_complete = true;
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(e)) if e.turn_id == "t2" => {
                t2_started_after_t1_complete = saw_t1_complete;
            }
            _ => {}
        }
    }
    assert!(saw_t1_complete, "t1 must still get TurnComplete");
    assert!(
        t2_started_after_t1_complete,
        "t1 TurnComplete must precede t2 TurnStarted"
    );
}

// ── H5: first UserMessage is real first prompt ────────────────────────────

#[test]
fn h5_first_user_message_is_true_first_prompt_not_band_text() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("compressed summary of old turns"),
            user_tail("m2", "later prompt"),
            assistant_text_tail("m3", "ok", None),
        ],
    };
    let messages = [
        msg(
            "m0",
            "t0",
            MessageKind::UserPrompt,
            1,
            "the real first prompt",
            None,
        ),
        msg(
            "m2",
            "t1",
            MessageKind::UserPrompt,
            10,
            "later prompt",
            None,
        ),
        msg("m3", "t1", MessageKind::AssistantText, 11, "ok", None),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m2", "m3"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);

    let first_um = items.iter().find_map(|i| match i {
        RolloutItem::EventMsg(EventMsg::UserMessage(e)) => Some(e.message.as_str()),
        _ => None,
    });
    assert_eq!(first_um, Some("the real first prompt"));
    assert!(!first_um.unwrap().contains("[context ·"));

    // Band text is in the model stream as ResponseItem only - no UserMessage twin.
    let band_twins: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            RolloutItem::EventMsg(EventMsg::UserMessage(e)) if e.message.contains("[context ·") => {
                Some(e.message.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(
        band_twins.is_empty(),
        "band entries must not emit display twins: {band_twins:?}"
    );
}

// ── tool-heavy + M8 success flag ──────────────────────────────────────────

#[test]
fn banded_thread_with_tool_heavy_tail_preserves_native_kinds() {
    let mut args = Map::new();
    args.insert("path".into(), json!("src/main.rs"));
    args.insert("__hostRaw".into(), json!("{\"path\":\"src/main.rs\"}"));

    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("compressed prior turns"),
            user_tail("m1", "read the file"),
            thinking_tail("m2", "I should use read_file"),
            tool_call_tail("m3", "fc_1", "read_file", args),
            tool_result_tail("m4", "fc_1", "read_file", "fn main() {}", None),
            assistant_text_tail("m5", "done", None),
        ],
    };
    let messages = [
        msg(
            "m1",
            "t1",
            MessageKind::UserPrompt,
            1,
            "read the file",
            None,
        ),
        msg(
            "m2",
            "t1",
            MessageKind::AssistantThinking,
            2,
            "I should use read_file",
            None,
        ),
        msg("m3", "t1", MessageKind::ToolCall, 3, "", None),
        msg_tool_result("m4", "t1", 4, "fc_1", "fn main() {}", false),
        msg(
            "m5",
            "t1",
            MessageKind::AssistantText,
            5,
            "done",
            Some(usage(125)),
        ),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3", "m4", "m5"],
        Some(TurnOutcome::Completed),
        None,
        Some("2026-07-01T12:00:00.000Z"),
        Some("2026-07-01T12:00:10.000Z"),
    )];

    let items = materialize(view, &messages, &turns, &[], Some(json!({})));
    let tail = tail_response_items(&items);

    assert!(tail.iter().any(|r| matches!(
        r,
        ResponseItem::FunctionCall { call_id, name, .. }
            if call_id == "fc_1" && name == "read_file"
    )));
    // M8: success Some(true) from stored isError=false.
    assert!(tail.iter().any(|r| matches!(
        r,
        ResponseItem::FunctionCallOutput { call_id, output, .. }
            if call_id == "fc_1"
                && matches!(&output.body, FunctionCallOutputBody::Text(t) if t == "fn main() {}")
                && output.success == Some(true)
    )));
    assert!(
        tail.iter()
            .any(|r| matches!(r, ResponseItem::Reasoning { .. }))
    );
}

// ── M6 runtime notes ──────────────────────────────────────────────────────

/// F1: fork compact-marker runtime notes are excluded from model + display
/// streams by structural key match (codex:{tid}:compact_marker:…), not text.
#[test]
fn f1_compact_marker_runtime_note_excluded_from_model_and_display() {
    use crate::COMPACT_MARKER_KEY_SEGMENT;
    use crate::is_compact_marker_idempotency_key;

    let marker_key = format!("codex:tid123:{COMPACT_MARKER_KEY_SEGMENT}:tip:1:0:fp");
    assert!(is_compact_marker_idempotency_key(&marker_key));
    assert!(!is_compact_marker_idempotency_key(
        "codex:tid123:id:msg_1:digest:runtime_note"
    ));

    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "live prompt"),
            SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
                // Body deliberately contains the marker text - exclusion must
                // still fire only via the key, not this string.
                content: "lhc_compact_marker {\"viewId\":\"v1\"}".into(),
                source_messages: vec![SessionThreadViewEntrySource {
                    message_id: "m-marker".into(),
                    idempotency_key: Some(marker_key),
                }],
            })),
            // Ordinary runtime note (no compact_marker key) still emits.
            SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
                content: "[runtime note] scaffolding".into(),
                source_messages: vec![SessionThreadViewEntrySource {
                    message_id: "m-rn".into(),
                    idempotency_key: Some("codex:tid123:anon:abc:0:runtime_note".into()),
                }],
            })),
        ],
    };
    let messages = [
        msg(
            "m-marker",
            "t0",
            MessageKind::RuntimeNote,
            1,
            "lhc_compact_marker {\"viewId\":\"v1\"}",
            None,
        ),
        msg("m1", "t1", MessageKind::UserPrompt, 2, "live prompt", None),
        msg(
            "m-rn",
            "t1",
            MessageKind::RuntimeNote,
            3,
            "scaffolding",
            None,
        ),
    ];
    let turns = [
        turn(
            "t0",
            0,
            &["m-marker"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t1",
            1,
            &["m1", "m-rn"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
    ];
    let items = materialize(view, &messages, &turns, &[], None);
    let tail = tail_response_items(&items);
    let texts: Vec<String> = tail
        .iter()
        .filter_map(|r| match r {
            ResponseItem::Message { content, .. } => Some(content_text(content)),
            _ => None,
        })
        .collect();
    assert!(
        !texts.iter().any(|t| t.contains("lhc_compact_marker")),
        "compact-marker note must not enter model stream: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "scaffolding"),
        "ordinary runtime_note still emits: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "live prompt"),
        "live user prompt still emits: {texts:?}"
    );
    // No display twin for the excluded marker either.
    let um: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            RolloutItem::EventMsg(EventMsg::UserMessage(e)) => Some(e.message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !um.iter().any(|t| t.contains("lhc_compact_marker")),
        "compact-marker must not appear in display stream: {um:?}"
    );
}

#[test]
fn m6_runtime_note_uses_stored_text_no_display_twin() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            // View adds the prefix; source is runtime_note.
            SessionThreadViewEntry::Message(SessionThreadViewMessage::User(SessionUserMessage {
                content: "[runtime note] host scaffolding".into(),
                source_messages: vec![SessionThreadViewEntrySource {
                    message_id: "m-rn".into(),
                    idempotency_key: None,
                }],
            })),
            user_tail("m1", "real prompt"),
        ],
    };
    let messages = [
        msg(
            "m-rn",
            "t0",
            MessageKind::RuntimeNote,
            1,
            "host scaffolding",
            None,
        ),
        msg("m1", "t1", MessageKind::UserPrompt, 2, "real prompt", None),
    ];
    let turns = [
        turn(
            "t0",
            0,
            &["m-rn"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
        turn(
            "t1",
            1,
            &["m1"],
            Some(TurnOutcome::Completed),
            None,
            None,
            None,
        ),
    ];
    let items = materialize(view, &messages, &turns, &[], None);
    let tail = tail_response_items(&items);
    assert!(
        tail.iter().any(|r| matches!(
            r,
            ResponseItem::Message { role, content, .. }
                if role == "user" && content_text(content) == "host scaffolding"
        )),
        "stored text without view prefix: {tail:?}"
    );
    // No UserMessage twin for the runtime note.
    let um_texts: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            RolloutItem::EventMsg(EventMsg::UserMessage(e)) => Some(e.message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !um_texts
            .iter()
            .any(|t| t.contains("host scaffolding") || t.contains("[runtime note]")),
        "runtime_note must not emit display twin: {um_texts:?}"
    );
}

// ── M11 payload content ───────────────────────────────────────────────────

#[test]
fn reverse_maps_local_shell_and_web_search_payloads() {
    let mut shell_args = Map::new();
    shell_args.insert("command".into(), json!(["echo", "hi"]));
    shell_args.insert("timeout_ms".into(), Value::Null);
    shell_args.insert("working_directory".into(), Value::Null);
    shell_args.insert("env".into(), Value::Null);
    shell_args.insert("user".into(), Value::Null);

    let mut ws_args = Map::new();
    ws_args.insert("type".into(), json!("search"));
    ws_args.insert("query".into(), json!("rust async"));

    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "run tools"),
            tool_call_tail("m2", "shell-1", "local_shell", shell_args),
            tool_call_tail("m3", "ws_1", "web_search", ws_args),
            tool_result_tail("m4", "shell-1", "local_shell", "hi\n", None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "run tools", None),
        msg("m2", "t1", MessageKind::ToolCall, 2, "", None),
        msg("m3", "t1", MessageKind::ToolCall, 3, "", None),
        msg_tool_result("m4", "t1", 4, "shell-1", "hi\n", false),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3", "m4"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);
    let tail = tail_response_items(&items);

    let shell = tail.iter().find_map(|r| match r {
        ResponseItem::LocalShellCall {
            action, call_id, ..
        } => Some((action, call_id)),
        _ => None,
    });
    assert!(shell.is_some());
    let (action, call_id) = shell.unwrap();
    assert_eq!(call_id.as_deref(), Some("shell-1"));
    match action {
        LocalShellAction::Exec(LocalShellExecAction { command, .. }) => {
            assert_eq!(command, &vec!["echo".to_string(), "hi".to_string()]);
        }
    }

    let ws = tail.iter().find_map(|r| match r {
        ResponseItem::WebSearchCall { action, .. } => action.as_ref(),
        _ => None,
    });
    assert!(matches!(
        ws,
        Some(WebSearchAction::Search {
            query: Some(q),
            ..
        }) if q == "rust async"
    ));
    assert!(items.iter().any(|i| matches!(
        i,
        RolloutItem::EventMsg(EventMsg::WebSearchEnd(e)) if e.query == "rust async"
    )));
    // local_shell tool RESULT asserted (not just the call).
    assert!(
        tail.iter().any(|r| matches!(
            r,
            ResponseItem::FunctionCallOutput { call_id, output, .. }
                if call_id == "shell-1"
                    && matches!(&output.body, FunctionCallOutputBody::Text(t) if t == "hi\n")
                    && output.success == Some(true)
        )),
        "local_shell result must reverse to FunctionCallOutput: {tail:?}"
    );
}

#[test]
fn tool_search_call_and_output_and_empty_tools_fallback() {
    let mut args = Map::new();
    args.insert("query".into(), json!("foo"));
    args.insert("__hostRaw".into(), json!("{\"query\":\"foo\"}"));

    // Valid tools JSON array.
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "search"),
            tool_call_tail("m2", "ts_1", "tool_search", args.clone()),
            tool_result_tail(
                "m3",
                "ts_1",
                "tool_search",
                r#"[{"name":"read_file"}]"#,
                None,
            ),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "search", None),
        msg("m2", "t1", MessageKind::ToolCall, 2, "", None),
        msg_tool_result("m3", "t1", 3, "ts_1", r#"[{"name":"read_file"}]"#, false),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);
    let tail = tail_response_items(&items);
    assert!(tail.iter().any(|r| matches!(
        r,
        ResponseItem::ToolSearchCall { call_id: Some(id), .. } if id == "ts_1"
    )));
    assert!(tail.iter().any(|r| matches!(
        r,
        ResponseItem::ToolSearchOutput { call_id: Some(id), tools, status, .. }
            if id == "ts_1" && tools.len() == 1 && status == "completed"
    )));

    // Non-array body -> unwrap_or_default empty tools vec.
    let view2 = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            user_tail("m1", "search"),
            tool_call_tail("m2", "ts_2", "tool_search", args),
            tool_result_tail("m3", "ts_2", "tool_search", "not-json-array", None),
        ],
    };
    let messages2 = [
        msg("m1", "t2", MessageKind::UserPrompt, 1, "search", None),
        msg("m2", "t2", MessageKind::ToolCall, 2, "", None),
        msg_tool_result("m3", "t2", 3, "ts_2", "not-json-array", false),
    ];
    let turns2 = [turn(
        "t2",
        1,
        &["m1", "m2", "m3"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items2 = materialize(view2, &messages2, &turns2, &[], None);
    let tail2 = tail_response_items(&items2);
    assert!(
        tail2.iter().any(|r| matches!(
            r,
            ResponseItem::ToolSearchOutput { call_id: Some(id), tools, .. }
                if id == "ts_2" && tools.is_empty()
        )),
        "empty-tools fallback when body is not a JSON array: {tail2:?}"
    );
}

// ── M12 realistic prior generation ────────────────────────────────────────

#[test]
fn m12_realistic_prior_generation_carry_forward_and_drops() {
    use codex_protocol::config_types::ApprovalsReviewer;
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Settings;
    use codex_protocol::items::PlanItem;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::PatchApplyEndEvent;
    use codex_protocol::protocol::PatchApplyStatus;
    use codex_protocol::protocol::ThreadSettingsAppliedEvent;
    use codex_protocol::protocol::ThreadSettingsSnapshot;

    let thread_id = ThreadId::default();
    let cwd = serde_json::from_value(json!("/tmp")).expect("AbsolutePathBuf");
    let settings = EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
        thread_settings: ThreadSettingsSnapshot {
            model: "gpt-settings".into(),
            model_provider_id: "openai".into(),
            service_tier: None,
            approval_policy: AskForApproval::Never,
            approvals_reviewer: ApprovalsReviewer::User,
            permission_profile: PermissionProfile::workspace_write(),
            active_permission_profile: None,
            cwd,
            reasoning_effort: None,
            reasoning_summary: None,
            personality: None,
            collaboration_mode: CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: "gpt-settings".into(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            },
        },
    });

    let prior = vec![
        RolloutItem::SessionMeta(empty_meta()),
        RolloutItem::ResponseItem(user_text_message("old prompt - must not copy")),
        RolloutItem::Compacted(CompactedItem {
            message: "old compact".into(),
            replacement_history: Some(vec![user_text_message("old hist")]),
            window_number: Some(0),
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
            info: Some(TokenUsageInfo {
                total_token_usage: TokenUsage {
                    total_tokens: 999,
                    ..Default::default()
                },
                last_token_usage: TokenUsage::default(),
                model_context_window: None,
            }),
            rate_limits: None,
        })),
        // Transient - must drop.
        RolloutItem::EventMsg(EventMsg::Error(ErrorEvent {
            message: "boom".into(),
            codex_error_info: None,
        })),
        // Carry-forward set.
        RolloutItem::EventMsg(settings),
        RolloutItem::EventMsg(EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
            thread_id,
            turn_id: None,
            goal: ThreadGoal {
                thread_id,
                objective: "ship it".into(),
                status: ThreadGoalStatus::Active,
                token_budget: None,
                tokens_used: 0,
                time_used_seconds: 0,
                created_at: 0,
                updated_at: 0,
            },
        })),
        RolloutItem::EventMsg(EventMsg::PatchApplyEnd(PatchApplyEndEvent {
            call_id: "patch-1".into(),
            turn_id: "t".into(),
            stdout: "ok".into(),
            stderr: String::new(),
            success: true,
            changes: Default::default(),
            status: PatchApplyStatus::Completed,
        })),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: "t".into(),
            item: TurnItem::Plan(PlanItem {
                id: "plan-1".into(),
                text: "do the thing".into(),
            }),
            started_at_ms: None,
            completed_at_ms: 0,
        })),
        RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true },
    ];

    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![band_entry("b"), user_tail("m1", "new")],
    };
    let messages = [msg("m1", "t1", MessageKind::UserPrompt, 1, "new", None)];
    let turns = [turn(
        "t1",
        1,
        &["m1"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &prior, None);

    // Goal carried.
    assert!(items.iter().any(|i| matches!(
        i,
        RolloutItem::EventMsg(EventMsg::ThreadGoalUpdated(e)) if e.goal.objective == "ship it"
    )));
    // Settings / patch / plan / inter-agent carried.
    assert!(
        items
            .iter()
            .any(|i| matches!(i, RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(_))))
    );
    assert!(items.iter().any(|i| matches!(
        i,
        RolloutItem::EventMsg(EventMsg::PatchApplyEnd(e)) if e.call_id == "patch-1"
    )));
    assert!(items.iter().any(|i| matches!(
        i,
        RolloutItem::EventMsg(EventMsg::ItemCompleted(e))
            if matches!(&e.item, TurnItem::Plan(p) if p.id == "plan-1")
    )));
    assert!(items.iter().any(|i| matches!(
        i,
        RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true }
    )));
    // Transients dropped.
    assert!(
        !items
            .iter()
            .any(|i| matches!(i, RolloutItem::EventMsg(EventMsg::Error(_))))
    );
    // Prior ResponseItems / Compacted / TokenCount not copied.
    assert!(!items.iter().any(|i| matches!(
        i,
        RolloutItem::ResponseItem(ResponseItem::Message { content, .. })
            if content_text(content).contains("old prompt")
    )));
    assert_eq!(
        items
            .iter()
            .filter(|i| matches!(i, RolloutItem::Compacted(_)))
            .count(),
        1
    );
    assert!(!items.iter().any(|i| matches!(
        i,
        RolloutItem::EventMsg(EventMsg::TokenCount(e))
            if e.info.as_ref().is_some_and(|info| info.total_token_usage.total_tokens == 999)
    )));
}

// ── M9 turn_context ───────────────────────────────────────────────────────

#[test]
fn m9_optional_turn_context_emitted_when_provided() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![band_entry("b")],
    };
    // Build a minimal TurnContext via serde to avoid constructing every field.
    let ctx: TurnContextItem = serde_json::from_value(json!({
        "cwd": "/",
        "approval_policy": "never",
        "sandbox_policy": {"type": "danger-full-access"},
        "model": "gpt-test",
        "summary": "auto",
    }))
    .expect("minimal TurnContextItem");
    let items = materialize_with_ctx(view, &[], &[], &[], None, Some(ctx));
    assert!(items.iter().any(|i| matches!(
        i,
        RolloutItem::TurnContext(c) if c.model == "gpt-test"
    )));
}

// ── M10 synthetic ids ─────────────────────────────────────────────────────

#[test]
fn m10_synthetic_ids_become_none_on_response_item_id() {
    // id-primary key whose item id is `synthetic:abc` (percent-encoded).
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("b"),
            SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(
                SessionAssistantMessage {
                    content: vec![SessionAssistantPart {
                        type_: SessionAssistantPartType::Text,
                        text: Some("hi".into()),
                        thinking: None,
                        thinking_signature: None,
                        tool_call_id: None,
                        tool_name: None,
                        arguments: None,
                    }],
                    source_messages: vec![SessionThreadViewEntrySource {
                        message_id: "m1".into(),
                        idempotency_key: Some(
                            "codex:t:id:synthetic%3Aabc:deadbeef:assistant_text".into(),
                        ),
                    }],
                    provider: None,
                    model: None,
                    api: None,
                },
            )),
        ],
    };
    let messages = [msg("m1", "t1", MessageKind::AssistantText, 1, "hi", None)];
    let turns = [turn(
        "t1",
        1,
        &["m1"],
        Some(TurnOutcome::Completed),
        None,
        None,
        None,
    )];
    let items = materialize(view, &messages, &turns, &[], None);
    let tail = tail_response_items(&items);
    assert!(tail.iter().any(|r| matches!(
        r,
        ResponseItem::Message { id: None, role, .. } if role == "assistant"
    )));
}

// ── L16 abort reason branches ─────────────────────────────────────────────

#[test]
fn abort_reason_maps_replac_review_budget() {
    assert_eq!(
        map_abort_reason(Some("replaced by newer turn")),
        TurnAbortReason::Replaced
    );
    assert_eq!(
        map_abort_reason(Some("review ended")),
        TurnAbortReason::ReviewEnded
    );
    assert_eq!(
        map_abort_reason(Some("budget limited")),
        TurnAbortReason::BudgetLimited
    );
    assert_eq!(
        map_abort_reason(Some("interrupted by user")),
        TurnAbortReason::Interrupted
    );
    assert_eq!(map_abort_reason(None), TurnAbortReason::Interrupted);
}

#[test]
fn aborted_turn_regenerates_turn_aborted_from_v5_outcome() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("band"),
            user_tail("m1", "do thing"),
            assistant_text_tail("m2", "partial", None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "do thing", None),
        msg("m2", "t1", MessageKind::AssistantText, 2, "partial", None),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2"],
        Some(TurnOutcome::Aborted),
        Some("interrupted by user"),
        Some("2026-07-01T12:00:00.000Z"),
        Some("2026-07-01T12:00:02.000Z"),
    )];
    let items = materialize(view, &messages, &turns, &[], None);
    assert!(items.iter().any(|i| matches!(
        i,
        RolloutItem::EventMsg(EventMsg::TurnAborted(e))
            if e.turn_id.as_deref() == Some("t1")
                && e.reason == TurnAbortReason::Interrupted
    )));
}

// ── pre-slice-A ───────────────────────────────────────────────────────────

#[test]
fn pre_slice_a_missing_host_facts_degrade_honestly() {
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![
            band_entry("band"),
            user_tail("m1", "hello"),
            assistant_text_tail("m2", "hi", None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, "hello", None),
        msg("m2", "t1", MessageKind::AssistantText, 2, "hi", None),
    ];
    let turns = [turn("t1", 1, &["m1", "m2"], None, None, None, None)];
    let items = materialize(view, &messages, &turns, &[], None);
    let complete = items.iter().find_map(|i| match i {
        RolloutItem::EventMsg(EventMsg::TurnComplete(e)) => Some(e),
        _ => None,
    });
    assert!(complete.is_some());
    assert_eq!(complete.unwrap().started_at, None);
    assert!(
        !items
            .iter()
            .any(|i| matches!(i, RolloutItem::EventMsg(EventMsg::TokenCount(_))))
    );
}

// ── adversarial ───────────────────────────────────────────────────────────

#[test]
fn adversarial_astral_unicode_boundary_floats_oversized_tool_result() {
    let astral = "hello 🌍 𝄞 中文 \u{1F980} boundary";
    let mut oversized = String::with_capacity(50_000);
    oversized.push_str("RESULT_START ");
    while oversized.len() < 40_000 {
        oversized.push_str("xy");
    }
    oversized.push_str(" RESULT_END");

    let usage_map = json!({
        "input_tokens": 1,
        "cached_input_tokens": 0,
        "cache_write_input_tokens": 0,
        "output_tokens": 2,
        "reasoning_output_tokens": 0,
        "total_tokens": 3,
    })
    .as_object()
    .cloned()
    .unwrap();

    let mut args = Map::new();
    args.insert("q".into(), json!(astral));
    args.insert(
        "__hostRaw".into(),
        json!(format!(
            "{{\"q\":{}}}",
            serde_json::to_string(astral).unwrap()
        )),
    );

    let view = SessionThreadView {
        thread_id: "adv".into(),
        entries: vec![
            band_entry(astral),
            user_tail("m1", astral),
            tool_call_tail("m2", "fc_adv", "search", args),
            tool_result_tail("m3", "fc_adv", "search", &oversized, None),
            assistant_text_tail("m4", astral, None),
        ],
    };
    let messages = [
        msg("m1", "t1", MessageKind::UserPrompt, 1, astral, None),
        msg("m2", "t1", MessageKind::ToolCall, 2, "", None),
        msg_tool_result("m3", "t1", 3, "fc_adv", &oversized, false),
        msg(
            "m4",
            "t1",
            MessageKind::AssistantText,
            4,
            astral,
            Some(usage_map),
        ),
    ];
    let turns = [turn(
        "t1",
        1,
        &["m1", "m2", "m3", "m4"],
        Some(TurnOutcome::Completed),
        None,
        Some("2026-07-01T00:00:00.000Z"),
        Some("2026-07-01T00:00:01.000Z"),
    )];
    let items = materialize(view, &messages, &turns, &[], Some(json!({"f": 1.0})));
    assert_eq!(boundary_completeness_error(&items), None);
    let tail = tail_response_items(&items);
    let out_content = tail.iter().find_map(|r| match r {
        ResponseItem::FunctionCallOutput { output, .. } => match &output.body {
            FunctionCallOutputBody::Text(t) => Some(t.as_str()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(out_content, Some(oversized.as_str()));
}

// ── iso + L15 id recovery ─────────────────────────────────────────────────

#[test]
fn iso_to_unix_secs_round_trips_host_format() {
    let iso = crate::mapping::unix_secs_to_iso(1_782_907_200);
    assert_eq!(iso_to_unix_secs(&iso), Some(1_782_907_200));
    assert_eq!(iso_to_unix_secs("1782907200"), Some(1_782_907_200));
}

#[test]
fn parse_host_id_from_key_recovers_real_ids() {
    assert_eq!(
        parse_host_id_from_key("codex:tid:id:msg_1:deadbeef:assistant_text"),
        Some("msg_1".into())
    );
    assert_eq!(
        parse_host_id_from_key("codex:tid:id:synthetic%3Aabc:deadbeef:tool_call"),
        None,
        "synthetic: must not become a ResponseItemId"
    );
}

// ── producer mutation targets (C1/H2/boundary live path) ──────────────────

#[test]
fn producer_emits_non_empty_replacement_history_and_window_number() {
    // Law 3: pins the *producer*, not just the checker.
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![band_entry("band body")],
    };
    let items = materialize(view, &[], &[], &[], None);
    let c = find_compacted(&items);
    assert!(
        c.replacement_history
            .as_ref()
            .is_some_and(|h| !h.is_empty()),
        "producer must emit non-empty replacement_history"
    );
    assert!(
        c.window_number.is_some(),
        "producer must emit window_number"
    );
    assert_eq!(boundary_completeness_error(&items), None);
}

#[test]
fn identity_match_reemits_encrypted_content() {
    let identity = ModelIdentity::new("openai", "gpt-test", ModelIdentity::RESPONSES_API);
    let mid = "m-sig";
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![SessionThreadViewEntry::Message(
            SessionThreadViewMessage::Assistant(SessionAssistantMessage {
                content: vec![SessionAssistantPart {
                    type_: SessionAssistantPartType::Thinking,
                    thinking: Some("plan".into()),
                    thinking_signature: Some("OPAQUE_SIG".into()),
                    text: None,
                    tool_call_id: None,
                    tool_name: None,
                    arguments: None,
                }],
                source_messages: vec![SessionThreadViewEntrySource {
                    message_id: mid.into(),
                    idempotency_key: Some("codex:t:id:rs_1:hash:assistant_thinking".into()),
                }],
                provider: Some("openai".into()),
                model: Some("gpt-test".into()),
                api: Some(ModelIdentity::RESPONSES_API.into()),
            }),
        )],
    };
    let messages: Vec<MessageRecord> = vec![];
    let turns: Vec<TurnRecord> = vec![];
    let result = materialize_rollout(&MaterializeInput {
        session_meta: empty_meta(),
        thread_view: &view,
        messages: &messages,
        turns: &turns,
        prior_generation: &[],
        boundary: boundary(1),
        world_state: None,
        turn_context: None,
        live_identity: Some(identity),
    });
    let reasoning = result
        .items
        .iter()
        .find_map(|it| match it {
            RolloutItem::ResponseItem(ResponseItem::Reasoning {
                encrypted_content,
                summary,
                ..
            }) => Some((encrypted_content.clone(), summary.clone())),
            _ => None,
        })
        .expect("expected Reasoning item");
    assert_eq!(reasoning.0.as_deref(), Some("OPAQUE_SIG"));
    assert!(
        !reasoning.1.is_empty(),
        "summary text should be preserved on match"
    );
}

#[test]
fn identity_mismatch_suppresses_encrypted_content() {
    let live = ModelIdentity::new("openai", "gpt-new", ModelIdentity::RESPONSES_API);
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![SessionThreadViewEntry::Message(
            SessionThreadViewMessage::Assistant(SessionAssistantMessage {
                content: vec![SessionAssistantPart {
                    type_: SessionAssistantPartType::Thinking,
                    thinking: Some("plan".into()),
                    thinking_signature: Some("OPAQUE_SIG".into()),
                    text: None,
                    tool_call_id: None,
                    tool_name: None,
                    arguments: None,
                }],
                source_messages: vec![SessionThreadViewEntrySource {
                    message_id: "m-sig2".into(),
                    idempotency_key: None,
                }],
                provider: Some("openai".into()),
                model: Some("gpt-old".into()),
                api: Some(ModelIdentity::RESPONSES_API.into()),
            }),
        )],
    };
    let result = materialize_rollout(&MaterializeInput {
        session_meta: empty_meta(),
        thread_view: &view,
        messages: &[],
        turns: &[],
        prior_generation: &[],
        boundary: boundary(1),
        world_state: None,
        turn_context: None,
        live_identity: Some(live),
    });
    let enc = result.items.iter().find_map(|it| match it {
        RolloutItem::ResponseItem(ResponseItem::Reasoning {
            encrypted_content, ..
        }) => Some(encrypted_content.clone()),
        _ => None,
    });
    assert_eq!(enc, Some(None), "mismatch must suppress encrypted_content");
}

#[test]
fn signature_only_thinking_emits_when_identity_matches() {
    let identity = ModelIdentity::new("openai", "gpt-test", ModelIdentity::RESPONSES_API);
    let view = SessionThreadView {
        thread_id: "t".into(),
        entries: vec![SessionThreadViewEntry::Message(
            SessionThreadViewMessage::Assistant(SessionAssistantMessage {
                content: vec![SessionAssistantPart {
                    type_: SessionAssistantPartType::Thinking,
                    thinking: None,
                    thinking_signature: Some("SIG_ONLY".into()),
                    text: None,
                    tool_call_id: None,
                    tool_name: None,
                    arguments: None,
                }],
                source_messages: vec![SessionThreadViewEntrySource {
                    message_id: "m-sig3".into(),
                    idempotency_key: None,
                }],
                provider: Some("openai".into()),
                model: Some("gpt-test".into()),
                api: Some(ModelIdentity::RESPONSES_API.into()),
            }),
        )],
    };
    let result = materialize_rollout(&MaterializeInput {
        session_meta: empty_meta(),
        thread_view: &view,
        messages: &[],
        turns: &[],
        prior_generation: &[],
        boundary: boundary(1),
        world_state: None,
        turn_context: None,
        live_identity: Some(identity),
    });
    let reasoning = result.items.iter().find_map(|it| match it {
        RolloutItem::ResponseItem(ResponseItem::Reasoning {
            encrypted_content,
            summary,
            ..
        }) => Some((encrypted_content.clone(), summary.clone())),
        _ => None,
    });
    assert!(
        reasoning.is_some(),
        "signature-only thinking must not be husked"
    );
    let (enc, summary) = reasoning.unwrap();
    assert_eq!(enc.as_deref(), Some("SIG_ONLY"));
    assert!(summary.is_empty(), "no text means empty summary");
}
