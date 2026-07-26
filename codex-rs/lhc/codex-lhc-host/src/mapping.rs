//! Exhaustive `ResponseItem` → `MessageEventInput` mapping for Codex.
//!
//! # Mapping table (17 `ResponseItem` variants)
//!
//! Classification of user-role messages is driven by
//! [`RawItemProvenance`](codex_extension_api::RawItemProvenance) — never by
//! text prefixes (FORK.md law 6).
//!
//! | Variant | LHC event_kind(s) | Payload / notes |
//! |---|---|---|
//! | `Message` + `UserPrompt` | `user_prompt` | `text` from content parts; full image/audio URLs in text (TextPayload is closed) |
//! | `Message` + `HostContext`/`InterAgent`/other | `runtime_note` | Host scaffolding or non-human origin |
//! | `Message` role=assistant | `assistant_text` | joined InputText + OutputText |
//! | `Message` role=developer/system | not captured | Host scaffolding meta |
//! | `AgentMessage` | `runtime_note` | author/recipient/parts folded into `text` |
//! | `Reasoning` summary/content | `assistant_thinking` | text |
//! | `Reasoning` encrypted | `assistant_thinking` | encrypted bytes verbatim as `text` |
//! | `LocalShellCall` | `tool_call` | by call_id |
//! | `FunctionCall` | `tool_call` | args object; verbatim wire string in `arguments.__hostRaw` (payload-only; never `extra`) |
//! | `ToolSearchCall` | `tool_call` | |
//! | `FunctionCallOutput` | `tool_result` | by call_id |
//! | `CustomToolCall` | `tool_call` | wire input in `arguments.__hostRaw` |
//! | `CustomToolCallOutput` | `tool_result` | |
//! | `ToolSearchOutput` | `tool_result` | |
//! | `WebSearchCall` | `tool_call` | |
//! | `ImageGenerationCall` | `tool_call` + `tool_result` | |
//! | `AdditionalTools` | `runtime_note` | tools JSON in `text` |
//! | `Compaction` | `runtime_note` | encrypted_content verbatim |
//! | `CompactionTrigger` | not captured | Request control |
//! | `ContextCompaction` | `runtime_note` if encrypted present, else not captured | |
//! | `Other` | not captured | |
//!
//! Exhaustive `match` — no `_ =>` arm.

use codex_extension_api::RawItemProvenance;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use lhc::intake_stream::MessageEventInput;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::idempotency::OccurrenceTracker;
use crate::idempotency::item_digest;
use crate::idempotency::item_event_key;
use crate::idempotency::item_stable_id;
use crate::idempotency::model_change_key;
use crate::idempotency::thinking_level_change_key;
use crate::idempotency::turn_end_key;

pub const ACTOR_USER: &str = "user";
pub const ACTOR_ASSISTANT: &str = "assistant";
pub const ACTOR_TOOL: &str = "tool";
pub const ACTOR_SYSTEM: &str = "system";
pub const HARNESS: &str = "codex";

/// One mapped LHC event ready for `message_events`.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedEvent {
    pub input: MessageEventInput,
}

/// Map a single `ResponseItem` into zero or more LHC events.
pub fn map_item(
    thread_id: &str,
    item: &ResponseItem,
    provenance: RawItemProvenance,
    tracker: &mut OccurrenceTracker,
) -> Vec<MappedEvent> {
    // Test-util: deliberate panic for a single fixture text so certification
    // can prove the worker catch_unwind continues (H9/I3). Content-keyed so
    // parallel tests do not share a static latch.
    #[cfg(any(test, feature = "test-util"))]
    if thread_id == "__lhc_test_panic_map__" {
        if let ResponseItem::Message { content, .. } = item {
            let panics = content.iter().any(|c| {
                matches!(
                    c,
                    ContentItem::InputText { text } if text == "this-map-panics-once"
                )
            });
            if panics {
                panic!("deliberate map_item panic for worker containment certification");
            }
        }
    }

    let digest = item_digest(item);
    let stable_id = item_stable_id(item);
    // Occurrence only advances on the anonymous path (no stable id).
    let occ = if stable_id.is_none() {
        tracker.next(&digest)
    } else {
        0
    };
    let sid = stable_id.as_deref();

    match item {
        ResponseItem::Message {
            role,
            content,
            id: _,
            phase: _,
            internal_chat_message_metadata_passthrough: _,
        } => map_message(thread_id, sid, &digest, occ, provenance, role, content),
        ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            id: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            // TextPayload is closed — fold structure into text (law 5: not prose
            // flattening of a live tool cycle; inter-agent is runtime_note).
            let mut texts = Vec::new();
            for part in content {
                match part {
                    AgentMessageInputContent::InputText { text } => texts.push(text.as_str()),
                    AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                        texts.push(encrypted_content.as_str());
                    }
                }
            }
            let body = texts.join("\n");
            let text = format!(
                "agent_message author={author} recipient={recipient} parts={}",
                serde_json::to_string(content).unwrap_or_else(|_| "[]".into())
            );
            let text = if body.is_empty() {
                text
            } else {
                format!("{text}\n{body}")
            };
            vec![text_event(
                thread_id,
                sid,
                &digest,
                occ,
                "runtime_note",
                ACTOR_SYSTEM,
                &text,
                None,
            )]
        }
        ResponseItem::Reasoning {
            summary,
            content,
            encrypted_content,
            id: _,
            internal_chat_message_metadata_passthrough: _,
        } => map_reasoning(
            thread_id,
            sid,
            &digest,
            occ,
            summary,
            content.as_deref(),
            encrypted_content.as_deref(),
        ),
        ResponseItem::LocalShellCall {
            call_id,
            action,
            status: _,
            id: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let tool_call_id = call_id
                .clone()
                .unwrap_or_else(|| format!("synthetic:{digest}"));
            let arguments = local_shell_arguments(action);
            vec![tool_call_event(
                thread_id,
                sid,
                &digest,
                occ,
                &tool_call_id,
                "local_shell",
                arguments,
            )]
        }
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            id: _,
            namespace: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let args = parse_arguments_object(arguments);
            vec![tool_call_event(
                thread_id, sid, &digest, occ, call_id, name, args,
            )]
        }
        ResponseItem::ToolSearchCall {
            call_id,
            arguments,
            execution: _,
            status: _,
            id: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let tool_call_id = call_id
                .clone()
                .unwrap_or_else(|| format!("synthetic:{digest}"));
            let mut args = value_to_arguments_map(arguments);
            args.insert(
                "__hostRaw".into(),
                json!(lhc::shared_tech::js_json::js_json_stringify(arguments)),
            );
            vec![tool_call_event(
                thread_id,
                sid,
                &digest,
                occ,
                &tool_call_id,
                "tool_search",
                args,
            )]
        }
        ResponseItem::FunctionCallOutput {
            call_id,
            output,
            id: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let (content, is_error) = function_output_content(output);
            vec![tool_result_event(
                thread_id, sid, &digest, occ, call_id, &content, is_error,
            )]
        }
        ResponseItem::CustomToolCall {
            call_id,
            name,
            input,
            id: _,
            status: _,
            namespace: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let args = parse_arguments_object(input);
            vec![tool_call_event(
                thread_id, sid, &digest, occ, call_id, name, args,
            )]
        }
        ResponseItem::CustomToolCallOutput {
            call_id,
            output,
            id: _,
            name: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let (content, is_error) = function_output_content(output);
            vec![tool_result_event(
                thread_id, sid, &digest, occ, call_id, &content, is_error,
            )]
        }
        ResponseItem::ToolSearchOutput {
            call_id,
            tools,
            status,
            execution: _,
            id: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let tool_call_id = call_id
                .clone()
                .unwrap_or_else(|| format!("synthetic:{digest}"));
            let content = serde_json::to_string(tools).unwrap_or_else(|_| "[]".into());
            let is_error = status == "failed" || status == "error";
            vec![tool_result_event(
                thread_id,
                sid,
                &digest,
                occ,
                &tool_call_id,
                &content,
                Some(is_error),
            )]
        }
        ResponseItem::WebSearchCall {
            id,
            status: _,
            action,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let tool_call_id = id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("synthetic:{digest}"));
            let arguments = web_search_arguments(action.as_ref());
            vec![tool_call_event(
                thread_id,
                sid,
                &digest,
                occ,
                &tool_call_id,
                "web_search",
                arguments,
            )]
        }
        ResponseItem::ImageGenerationCall {
            id,
            status,
            revised_prompt,
            result,
            internal_chat_message_metadata_passthrough: _,
        } => {
            let tool_call_id = id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("synthetic:{digest}"));
            let mut args = Map::new();
            if let Some(p) = revised_prompt {
                args.insert("revisedPrompt".into(), json!(p));
            }
            let mut out = vec![tool_call_event(
                thread_id,
                sid,
                &digest,
                occ,
                &tool_call_id,
                "image_generation",
                args,
            )];
            let result_content = json!({
                "status": status,
                "result": result,
                "revisedPrompt": revised_prompt,
            })
            .to_string();
            out.push(tool_result_event(
                thread_id,
                sid,
                &digest,
                occ,
                &tool_call_id,
                &result_content,
                Some(status == "failed" || status == "error"),
            ));
            out
        }
        ResponseItem::AdditionalTools {
            tools,
            role: _,
            id: _,
        } => {
            let text = format!(
                "additional_tools {}",
                serde_json::to_string(tools).unwrap_or_else(|_| "[]".into())
            );
            vec![text_event(
                thread_id,
                sid,
                &digest,
                occ,
                "runtime_note",
                ACTOR_SYSTEM,
                &text,
                None,
            )]
        }
        ResponseItem::Compaction {
            encrypted_content,
            id: _,
            internal_chat_message_metadata_passthrough: _,
        } => vec![text_event(
            thread_id,
            sid,
            &digest,
            occ,
            "runtime_note",
            ACTOR_SYSTEM,
            encrypted_content,
            Some("compaction"),
        )],
        ResponseItem::CompactionTrigger {} => Vec::new(),
        ResponseItem::ContextCompaction {
            encrypted_content,
            id: _,
            internal_chat_message_metadata_passthrough: _,
        } => {
            // Not captured when empty — no empty-note placeholder.
            match encrypted_content.as_deref().filter(|s| !s.is_empty()) {
                Some(text) => vec![text_event(
                    thread_id,
                    sid,
                    &digest,
                    occ,
                    "runtime_note",
                    ACTOR_SYSTEM,
                    text,
                    Some("context_compaction"),
                )],
                None => Vec::new(),
            }
        }
        ResponseItem::Other => Vec::new(),
    }
}

/// Host-observed facts for a `turn_end` payload (schema v5 / D1–D2).
///
/// All fields optional — empty payload remains valid for hosts that omit them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnEndFacts {
    /// `"completed"` or `"aborted"`.
    pub outcome: Option<&'static str>,
    pub outcome_reason: Option<String>,
    /// Host wall-clock start (ISO-8601 UTC).
    pub started_at: Option<String>,
    /// Host wall-clock end (ISO-8601 UTC).
    pub ended_at: Option<String>,
}

/// Convert host Unix-seconds timestamps (as on TurnStarted/Complete/Aborted)
/// into ISO-8601 UTC strings for the LHC `startedAt`/`endedAt` payload fields.
pub fn unix_secs_to_iso(secs: i64) -> String {
    match chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0) {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%S.000Z").to_string(),
        None => format!("{secs}"),
    }
}

/// Map a host turn boundary into a `turn_end` event.
///
/// `reason` is only the idempotency-key discriminator (`completed`/`aborted`/
/// `error`/`stop` legacy); payload host facts ride `facts`.
pub fn map_turn_end(
    thread_id: &str,
    turn_id: &str,
    reason: &str,
    facts: &TurnEndFacts,
) -> MappedEvent {
    let key = turn_end_key(thread_id, turn_id, reason);
    let mut payload = Map::new();
    if let Some(outcome) = facts.outcome {
        payload.insert("outcome".into(), json!(outcome));
    }
    if let Some(reason) = facts.outcome_reason.as_ref() {
        payload.insert("outcomeReason".into(), json!(reason));
    }
    if let Some(started) = facts.started_at.as_ref() {
        payload.insert("startedAt".into(), json!(started));
    }
    if let Some(ended) = facts.ended_at.as_ref() {
        payload.insert("endedAt".into(), json!(ended));
    }
    MappedEvent {
        input: MessageEventInput {
            event_kind: "turn_end".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR_ASSISTANT.to_string(),
            harness: HARNESS.to_string(),
            payload,
            extra: Map::new(),
        },
    }
}

/// Serialize a host `TokenUsage` into a free-form JSON object for
/// `assistant_text.providerUsage` (verbatim; no field filter).
pub fn token_usage_to_provider_usage(
    usage: &codex_protocol::protocol::TokenUsage,
) -> Option<Map<String, Value>> {
    match serde_json::to_value(usage) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Attach optional `providerUsage` to a mapped `assistant_text` event.
pub fn attach_provider_usage(event: &mut MappedEvent, usage: &Map<String, Value>) {
    if event.input.event_kind != "assistant_text" {
        return;
    }
    event
        .input
        .payload
        .insert("providerUsage".into(), Value::Object(usage.clone()));
}

/// Host-injected runtime note (degradation / truncation markers). Key is not
/// content-addressed — each latch uses a distinct `key_suffix`.
pub fn map_runtime_note(thread_id: &str, text: &str, key_suffix: &str) -> MappedEvent {
    let tid = crate::idempotency::encode_thread_id(thread_id);
    let suffix = crate::idempotency::encode_thread_id(key_suffix);
    let key = format!("codex:{tid}:runtime_note:host:{suffix}");
    let mut payload = Map::new();
    payload.insert("text".into(), json!(text));
    MappedEvent {
        input: MessageEventInput {
            event_kind: "runtime_note".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR_SYSTEM.to_string(),
            harness: HARNESS.to_string(),
            payload,
            extra: Map::new(),
        },
    }
}

/// Map model and/or thinking-level transitions into LHC events.
///
/// Emits only on actual field change. Keys are transition-stable (prev→new)
/// so a resume re-fire of the same transition is deduped by LHC.
pub fn map_model_or_thinking_change(
    thread_id: &str,
    previous_model: &str,
    new_model: &str,
    previous_level: &str,
    new_level: &str,
) -> Vec<MappedEvent> {
    let mut out = Vec::new();
    if previous_model != new_model {
        let key = model_change_key(thread_id, previous_model, new_model);
        let mut payload = Map::new();
        payload.insert("previousModel".into(), json!(previous_model));
        payload.insert("newModel".into(), json!(new_model));
        out.push(MappedEvent {
            input: MessageEventInput {
                event_kind: "model_change".to_string(),
                idempotency_key: Some(key),
                actor: ACTOR_SYSTEM.to_string(),
                harness: HARNESS.to_string(),
                payload,
                extra: Map::new(),
            },
        });
    }
    if previous_level != new_level {
        let key = thinking_level_change_key(thread_id, previous_level, new_level);
        let mut payload = Map::new();
        payload.insert("previousLevel".into(), json!(previous_level));
        payload.insert("newLevel".into(), json!(new_level));
        out.push(MappedEvent {
            input: MessageEventInput {
                event_kind: "thinking_level_change".to_string(),
                idempotency_key: Some(key),
                actor: ACTOR_SYSTEM.to_string(),
                harness: HARNESS.to_string(),
                payload,
                extra: Map::new(),
            },
        });
    }
    out
}

fn map_message(
    thread_id: &str,
    sid: Option<&str>,
    digest: &str,
    occ: u64,
    provenance: RawItemProvenance,
    role: &str,
    content: &[ContentItem],
) -> Vec<MappedEvent> {
    match role {
        "developer" | "system" => Vec::new(),
        "user" => {
            let text = content_items_text(content);
            if text.is_empty() {
                return Vec::new();
            }
            // Typed provenance — exhaustive, no content-prefix classifier.
            let (kind, actor) = match provenance {
                RawItemProvenance::UserPrompt => ("user_prompt", ACTOR_USER),
                RawItemProvenance::HostContext
                | RawItemProvenance::InterAgent
                | RawItemProvenance::ModelOutput => ("runtime_note", ACTOR_SYSTEM),
            };
            vec![text_event(
                thread_id, sid, digest, occ, kind, actor, &text, None,
            )]
        }
        "assistant" => {
            let text = content_items_text(content);
            if text.is_empty() {
                return Vec::new();
            }
            vec![text_event(
                thread_id,
                sid,
                digest,
                occ,
                "assistant_text",
                ACTOR_ASSISTANT,
                &text,
                None,
            )]
        }
        other => {
            let text = content_items_text(content);
            vec![text_event(
                thread_id,
                sid,
                digest,
                occ,
                "runtime_note",
                ACTOR_SYSTEM,
                &format!("[{other}] {text}"),
                None,
            )]
        }
    }
}

fn map_reasoning(
    thread_id: &str,
    sid: Option<&str>,
    digest: &str,
    occ: u64,
    summary: &[ReasoningItemReasoningSummary],
    content: Option<&[ReasoningItemContent]>,
    encrypted_content: Option<&str>,
) -> Vec<MappedEvent> {
    let mut out = Vec::new();
    let summary_text = reasoning_summary_text(summary, content);
    let has_encrypted = encrypted_content.is_some_and(|s| !s.is_empty());

    if !summary_text.is_empty() {
        let part = if has_encrypted { Some("summary") } else { None };
        out.push(text_event(
            thread_id,
            sid,
            digest,
            occ,
            "assistant_thinking",
            ACTOR_ASSISTANT,
            &summary_text,
            part,
        ));
    }
    if let Some(enc) = encrypted_content.filter(|s| !s.is_empty()) {
        let part = if summary_text.is_empty() {
            None
        } else {
            Some("encrypted")
        };
        out.push(text_event(
            thread_id,
            sid,
            digest,
            occ,
            "assistant_thinking",
            ACTOR_ASSISTANT,
            enc,
            part,
        ));
    }
    out
}

fn reasoning_summary_text(
    summary: &[ReasoningItemReasoningSummary],
    content: Option<&[ReasoningItemContent]>,
) -> String {
    let mut parts = Vec::new();
    for s in summary {
        match s {
            ReasoningItemReasoningSummary::SummaryText { text } => parts.push(text.as_str()),
        }
    }
    if let Some(content) = content {
        for c in content {
            match c {
                ReasoningItemContent::ReasoningText { text }
                | ReasoningItemContent::Text { text } => {
                    parts.push(text.as_str());
                }
            }
        }
    }
    parts.join("")
}

/// Build text with full media URLs inline (TextPayload is closed — only `text`).
/// No silent truncation (H1/F8): URLs pass through verbatim inside the text field.
fn content_items_text(content: &[ContentItem]) -> String {
    let mut chunks = Vec::with_capacity(content.len());
    for part in content {
        match part {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                chunks.push(text.clone());
            }
            ContentItem::InputImage {
                image_url,
                detail: _,
            } => {
                chunks.push(format!("[image:{image_url}]"));
            }
            ContentItem::InputAudio { audio_url } => {
                chunks.push(format!("[audio:{audio_url}]"));
            }
        }
    }
    chunks.join("\n")
}

fn local_shell_arguments(action: &LocalShellAction) -> Map<String, Value> {
    match action {
        LocalShellAction::Exec(exec) => serde_json::to_value(exec)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
    }
}

fn web_search_arguments(action: Option<&WebSearchAction>) -> Map<String, Value> {
    match action {
        Some(a) => serde_json::to_value(a)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
        None => Map::new(),
    }
}

fn function_output_content(output: &FunctionCallOutputPayload) -> (String, Option<bool>) {
    let content = match &output.body {
        FunctionCallOutputBody::Text(s) => s.clone(),
        FunctionCallOutputBody::ContentItems(items) => content_items_output_text(items),
    };
    let is_error = output.success.map(|ok| !ok);
    (content, is_error)
}

fn content_items_output_text(items: &[FunctionCallOutputContentItem]) -> String {
    let mut chunks = Vec::new();
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => chunks.push(text.clone()),
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: _,
            } => {
                chunks.push(format!("[image:{image_url}]"));
            }
            FunctionCallOutputContentItem::InputAudio { audio_url } => {
                chunks.push(format!("[audio:{audio_url}]"));
            }
            FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
                chunks.push(encrypted_content.clone());
            }
        }
    }
    chunks.join("\n")
}

fn parse_arguments_object(raw: &str) -> Map<String, Value> {
    // LHC ToolCallPayload is closed: only toolCallId/toolName/arguments.
    // Verbatim wire string lives *inside* the arguments map as `__hostRaw`.
    // Never use MessageEventInput.extra — envelope validation rejects it (H1).
    let mut map = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => map,
        Ok(other) => {
            let mut m = Map::new();
            m.insert("value".into(), other);
            m
        }
        Err(_) => {
            let mut m = Map::new();
            m.insert("raw".into(), json!(raw));
            m
        }
    };
    map.insert("__hostRaw".into(), json!(raw));
    map
}

fn value_to_arguments_map(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = Map::new();
            map.insert("value".into(), other.clone());
            map
        }
    }
}

fn text_event(
    thread_id: &str,
    sid: Option<&str>,
    digest: &str,
    occ: u64,
    kind: &str,
    actor: &str,
    text: &str,
    part: Option<&str>,
) -> MappedEvent {
    let key = item_event_key(thread_id, sid, digest, occ, kind, part);
    let mut payload = Map::new();
    payload.insert("text".into(), json!(text));
    MappedEvent {
        input: MessageEventInput {
            event_kind: kind.to_string(),
            idempotency_key: Some(key),
            actor: actor.to_string(),
            harness: HARNESS.to_string(),
            payload,
            extra: Map::new(),
        },
    }
}

fn tool_call_event(
    thread_id: &str,
    sid: Option<&str>,
    digest: &str,
    occ: u64,
    tool_call_id: &str,
    tool_name: &str,
    arguments: Map<String, Value>,
) -> MappedEvent {
    let key = item_event_key(thread_id, sid, digest, occ, "tool_call", Some(tool_call_id));
    let mut payload = Map::new();
    payload.insert("toolCallId".into(), json!(tool_call_id));
    payload.insert("toolName".into(), json!(tool_name));
    payload.insert("arguments".into(), Value::Object(arguments));
    MappedEvent {
        input: MessageEventInput {
            event_kind: "tool_call".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR_ASSISTANT.to_string(),
            harness: HARNESS.to_string(),
            payload,
            extra: Map::new(),
        },
    }
}

fn tool_result_event(
    thread_id: &str,
    sid: Option<&str>,
    digest: &str,
    occ: u64,
    tool_call_id: &str,
    content: &str,
    is_error: Option<bool>,
) -> MappedEvent {
    let key = item_event_key(
        thread_id,
        sid,
        digest,
        occ,
        "tool_result",
        Some(tool_call_id),
    );
    let mut payload = Map::new();
    payload.insert("toolCallId".into(), json!(tool_call_id));
    payload.insert("content".into(), json!(content));
    if let Some(err) = is_error {
        payload.insert("isError".into(), json!(err));
    }
    MappedEvent {
        input: MessageEventInput {
            event_kind: "tool_result".to_string(),
            idempotency_key: Some(key),
            actor: ACTOR_TOOL.to_string(),
            harness: HARNESS.to_string(),
            payload,
            extra: Map::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ResponseItemId;
    use pretty_assertions::assert_eq;

    fn tracker() -> OccurrenceTracker {
        OccurrenceTracker::new()
    }

    #[test]
    fn user_prompt_provenance_maps_to_user_prompt() {
        let item = ResponseItem::Message {
            id: Some(ResponseItemId::from_server("msg_1".into())),
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "<user_instructions> what does this tag mean?".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let events = map_item("t", &item, RawItemProvenance::UserPrompt, &mut tracker());
        assert_eq!(events[0].input.event_kind, "user_prompt");
    }

    #[test]
    fn host_context_user_role_is_runtime_note() {
        let item = ResponseItem::Message {
            id: Some(ResponseItemId::from_server("msg_ctx".into())),
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "RolloutBudgetContext stuff".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let events = map_item("t", &item, RawItemProvenance::HostContext, &mut tracker());
        assert_eq!(events[0].input.event_kind, "runtime_note");
    }

    #[test]
    fn same_id_replays_identical_key() {
        let item = ResponseItem::Message {
            id: Some(ResponseItemId::from_server("msg_stable".into())),
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "hello".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let mut t = tracker();
        let e1 = map_item("t", &item, RawItemProvenance::UserPrompt, &mut t);
        let e2 = map_item("t", &item, RawItemProvenance::UserPrompt, &mut t);
        assert_eq!(
            e1[0].input.idempotency_key, e2[0].input.idempotency_key,
            "restart re-presentation of same ResponseItemId must collide"
        );
    }

    #[test]
    fn function_call_preserves_arguments_raw_bytes() {
        let raw = r#"{ "a" : 1 ,  "b" : 2 }"#;
        let item = ResponseItem::FunctionCall {
            id: Some(ResponseItemId::from_server("fc_1".into())),
            name: "x".into(),
            namespace: None,
            arguments: raw.into(),
            call_id: "c1".into(),
            internal_chat_message_metadata_passthrough: None,
        };
        let events = map_item("t", &item, RawItemProvenance::ModelOutput, &mut tracker());
        let args = events[0]
            .input
            .payload
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("arguments object");
        assert_eq!(
            args.get("__hostRaw"),
            Some(&json!(raw)),
            "verbatim wire string must land in arguments.__hostRaw"
        );
        assert!(
            events[0].input.extra.is_empty(),
            "extra must stay empty (H1)"
        );
    }

    #[test]
    fn function_call_duplicate_keys_raw_preserved() {
        let raw = r#"{"k":1,"k":2}"#;
        let item = ResponseItem::FunctionCall {
            id: None,
            name: "x".into(),
            namespace: None,
            arguments: raw.into(),
            call_id: "c2".into(),
            internal_chat_message_metadata_passthrough: None,
        };
        let events = map_item("t", &item, RawItemProvenance::ModelOutput, &mut tracker());
        let args = events[0]
            .input
            .payload
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("arguments");
        assert_eq!(args.get("__hostRaw"), Some(&json!(raw)));
    }

    #[test]
    fn image_url_full_in_text_payload() {
        let url = format!("data:image/png;base64,{}", "A".repeat(4000));
        let item = ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputImage {
                image_url: url.clone(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let events = map_item("t", &item, RawItemProvenance::UserPrompt, &mut tracker());
        let text = events[0]
            .input
            .payload
            .get("text")
            .and_then(|v| v.as_str())
            .expect("text");
        assert!(
            text.contains(&url),
            "full image URL must be in text payload, got {} chars",
            text.len()
        );
        assert!(events[0].input.extra.is_empty());
    }

    #[test]
    fn reasoning_encrypted_passthrough() {
        let secret = "enc-blob-verbatim-Ω";
        let item = ResponseItem::Reasoning {
            id: Some(ResponseItemId::from_server("r1".into())),
            summary: vec![],
            content: None,
            encrypted_content: Some(secret.into()),
            internal_chat_message_metadata_passthrough: None,
        };
        let events = map_item("t", &item, RawItemProvenance::ModelOutput, &mut tracker());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].input.payload.get("text"), Some(&json!(secret)));
    }

    #[test]
    fn model_change_only_on_actual_diff() {
        let none = map_model_or_thinking_change("t", "m", "m", "high", "high");
        assert!(none.is_empty());
        let events = map_model_or_thinking_change("t", "m1", "m2", "low", "high");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].input.event_kind, "model_change");
        assert_eq!(events[1].input.event_kind, "thinking_level_change");
        // Same transition collides (restart re-fire stability).
        let again = map_model_or_thinking_change("t", "m1", "m2", "low", "high");
        assert_eq!(
            events[0].input.idempotency_key,
            again[0].input.idempotency_key
        );
    }

    #[test]
    fn turn_end_empty_payload_when_no_facts() {
        let event = map_turn_end("t", "turn-1", "stop", &TurnEndFacts::default());
        assert_eq!(event.input.event_kind, "turn_end");
        assert!(event.input.payload.is_empty());
    }

    #[test]
    fn turn_end_carries_v5_host_facts() {
        let facts = TurnEndFacts {
            outcome: Some("aborted"),
            outcome_reason: Some("interrupted".into()),
            started_at: Some("2026-07-01T12:00:00.000Z".into()),
            ended_at: Some("2026-07-01T12:00:04.000Z".into()),
        };
        let event = map_turn_end("t", "turn-1", "aborted", &facts);
        assert_eq!(event.input.payload.get("outcome"), Some(&json!("aborted")));
        assert_eq!(
            event.input.payload.get("outcomeReason"),
            Some(&json!("interrupted"))
        );
        assert_eq!(
            event.input.payload.get("startedAt"),
            Some(&json!("2026-07-01T12:00:00.000Z"))
        );
        assert_eq!(
            event.input.payload.get("endedAt"),
            Some(&json!("2026-07-01T12:00:04.000Z"))
        );
        assert!(event.input.extra.is_empty());
    }

    #[test]
    fn provider_usage_attaches_only_to_assistant_text() {
        let item = ResponseItem::Message {
            id: Some(ResponseItemId::from_server("a1".into())),
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "hello".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let mut events = map_item("t", &item, RawItemProvenance::ModelOutput, &mut tracker());
        assert_eq!(events.len(), 1);
        let usage = json!({"input_tokens": 11, "output_tokens": 3})
            .as_object()
            .cloned()
            .unwrap();
        attach_provider_usage(&mut events[0], &usage);
        assert_eq!(
            events[0].input.payload.get("providerUsage"),
            Some(&Value::Object(usage))
        );
    }

    #[test]
    fn partial_image_gen_keys_stable_for_retry() {
        let item = ResponseItem::ImageGenerationCall {
            id: Some(ResponseItemId::from_server("ig_probe".into())),
            status: "completed".into(),
            revised_prompt: Some("a cat".into()),
            result: "base64".into(),
            internal_chat_message_metadata_passthrough: None,
        };
        let mut t = tracker();
        let e1 = map_item("t", &item, RawItemProvenance::ModelOutput, &mut t);
        let e2 = map_item("t", &item, RawItemProvenance::ModelOutput, &mut t);
        assert_eq!(e1.len(), 2);
        assert_eq!(e1[0].input.idempotency_key, e2[0].input.idempotency_key);
        assert_eq!(e1[1].input.idempotency_key, e2[1].input.idempotency_key);
    }
}
