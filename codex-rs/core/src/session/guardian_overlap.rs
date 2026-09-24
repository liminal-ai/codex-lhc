//! Proven overlap between a Guardian checkpoint and a Compacted suffix.
//!
//! Replay finds the longest `k` where `checkpoint[-k:]` is the same sequence as
//! `suffix[:k]`. A window is proven only when every pair is compatible **and**
//! at least one pair shares an id or call_id (the anchor).
//!
//! Compatible pairs:
//! - both sides carry the same identity token (variant + id/`call_id`);
//! - or both lack identity and the message/reasoning content matches.
//!
//! Content-only windows (a repeated "continue" with `id: None`) are unproven.
//! Unproven suffix items are replayed. Duplicates are allowed; omitting a new
//! instruction or tool result is not.
//!
//! Full [`ResponseItem`] `PartialEq` is not used: LHC materialize rebuilds
//! native shapes (new ids, dropped namespace/phase) that are not equal to the
//! live checkpoint items even when they are the same turn.

use codex_protocol::models::ResponseItem;

pub(super) fn proven_suffix_overlap(checkpoint: &[ResponseItem], suffix: &[ResponseItem]) -> usize {
    let max = checkpoint.len().min(suffix.len());
    for k in (1..=max).rev() {
        if window_is_proven(&checkpoint[checkpoint.len() - k..], &suffix[..k]) {
            return k;
        }
    }
    0
}

fn window_is_proven(left: &[ResponseItem], right: &[ResponseItem]) -> bool {
    let mut anchored = false;
    for (left, right) in left.iter().zip(right) {
        match (identity_token(left), identity_token(right)) {
            (Some(left_id), Some(right_id)) if left_id == right_id => anchored = true,
            (None, None) if content_equivalent(left, right) => {}
            _ => return false,
        }
    }
    anchored
}

fn identity_token(item: &ResponseItem) -> Option<String> {
    let kind = kind_tag(item);
    if let Some(call_id) = call_id(item) {
        return Some(format!("{kind}:call:{call_id}"));
    }
    item.id().map(|id| format!("{kind}:id:{id}"))
}

fn kind_tag(item: &ResponseItem) -> &'static str {
    match item {
        ResponseItem::Message { role, .. } if role == "user" => "user",
        ResponseItem::Message { .. } => "assistant",
        ResponseItem::AgentMessage { .. } => "agent_message",
        ResponseItem::FunctionCall { .. } => "function_call",
        ResponseItem::FunctionCallOutput { .. } => "function_call_output",
        ResponseItem::CustomToolCall { .. } => "custom_tool_call",
        ResponseItem::CustomToolCallOutput { .. } => "custom_tool_call_output",
        ResponseItem::LocalShellCall { .. } => "local_shell_call",
        ResponseItem::ToolSearchCall { .. } => "tool_search_call",
        ResponseItem::ToolSearchOutput { .. } => "tool_search_output",
        ResponseItem::WebSearchCall { .. } => "web_search_call",
        ResponseItem::ImageGenerationCall { .. } => "image_generation_call",
        ResponseItem::Reasoning { .. } => "reasoning",
        ResponseItem::AdditionalTools { .. } => "additional_tools",
        ResponseItem::Compaction { .. } => "compaction",
        ResponseItem::ContextCompaction { .. } => "context_compaction",
        ResponseItem::ConfigurationUpdate { .. } => "configuration_update",
        ResponseItem::CompactionTrigger { .. } => "compaction_trigger",
        ResponseItem::Other => "other",
    }
}

fn call_id(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.clone()),
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::LocalShellCall { call_id, .. }
        | ResponseItem::ToolSearchCall { call_id, .. }
        | ResponseItem::ToolSearchOutput { call_id, .. } => call_id.clone(),
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Message { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::ConfigurationUpdate { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::Other => None,
    }
}

fn content_equivalent(left: &ResponseItem, right: &ResponseItem) -> bool {
    match (left, right) {
        (
            ResponseItem::Message {
                role: left_role,
                content: left_content,
                ..
            },
            ResponseItem::Message {
                role: right_role,
                content: right_content,
                ..
            },
        ) => left_role == right_role && left_content == right_content,
        (
            ResponseItem::Reasoning {
                summary: left_summary,
                content: left_content,
                ..
            },
            ResponseItem::Reasoning {
                summary: right_summary,
                content: right_content,
                ..
            },
        ) => left_summary == right_summary && left_content == right_content,
        _ => false,
    }
}

#[cfg(test)]
#[path = "guardian_overlap_tests.rs"]
mod tests;
