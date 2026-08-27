//! Rollout materializer — pure function from LHC read surfaces to a complete
//! rollout item sequence (slice B of the rollout rework).
//!
//! # Output structure
//!
//! ```text
//! SessionMeta
//!   → carry-forward settings / goal (if any)
//!   → first UserMessage display twin (true first user_prompt; display-only)
//!   → model stream: banded history as text ResponseItems (NO display twins)
//!   → exactly ONE Compacted { replacement_history, window_number, window ids }
//!   → one full WorldState snapshot (when supplied)
//!   → ContextCompacted display marker
//!   → optional TurnContext (when supplied — previous_turn_settings recovery)
//!   → post-boundary: native ResponseItems + lifecycle / TokenCount /
//!     message-reasoning twins (rollback applied by exclusion, not marker)
//!   → carry-forward review / patch / MCP / subagent / plan-sleep /
//!     inter-agent items
//! ```
//!
//! No file IO. Slice C wires this into the compact arm.
//!
//! # Law (FORK.md + plan)
//!
//! Banded history compresses to text messages — that *is* compaction.
//! Post-boundary (tail) content is rebuilt as **native** `ResponseItem`s from
//! typed LHC entries (tool calls with call ids, paired results, reasoning) —
//! never flattened to prose. Classification uses source-message kinds, never
//! rendered text prefixes (law 6). [`crate::mapping`] is the forward map; this
//! module is the reverse for the tail.
//!
//! # Display-stream disposition (Legacy mode — terminal Codex)
//!
//! | Event class | Disposition | Source |
//! |---|---|---|
//! | `TurnStarted` / `TurnComplete` / `TurnAborted` | **Regenerated** (post-boundary closed turns only; open turns emit nothing) | `turns` v5 |
//! | `TokenCount` | **Regenerated** | `last` = per-call `provider_usage`; `total` = cumulative sum up to that call |
//! | `UserMessage` (first) | **Regenerated display-only** from the thread's true first `user_prompt` (not band text) | messages record |
//! | `UserMessage` / `AgentMessage` / `AgentReasoning` (tail) | **Regenerated** from native tail ResponseItems | model stream tail |
//! | `AgentReasoningRawContent` | **Conditional** | encrypted_content re-emitted when stored identity matches live model (R2) |
//! | `ThreadSettingsApplied` / `ThreadGoalUpdated` | **Carried forward** | prior generation |
//! | `ThreadRolledBack` | **Applied, not carried** | prior markers drop those user turns from the tail; no marker in the rebuilt file |
//! | `ContextCompacted` | **Regenerated** | once at the boundary after `Compacted` |
//! | `WebSearchEnd` / `ImageGenerationEnd` | **Regenerated** when tail holds native tools | reverse-mapped tail |
//! | `ItemCompleted(Plan\|Sleep)` / `InterAgentCommunication{,Metadata}` | **Carried forward** | prior generation |
//! | Review / patch / MCP / subagent ends | **Carried forward** when present | prior generation |
//! | Transient (`Error`, `ExecCommandEnd`, collab, realtime, …) | **Dropped** | never persisted |
//! | Fork compact-marker `runtime_note` | **Excluded** (model + display) | idempotency key namespace `codex:{tid}:compact_marker:…` — fork bookkeeping; boundary `Compacted` already records that a compact happened. Keys matched structurally ([`crate::is_compact_marker_idempotency_key`]), never by note text (law 6). Rows stay in the LHC record as provenance. |
//!
//! # Capture gaps (do not invent content)
//!
//! See [`CAPTURE_GAPS`]. Missing host facts degrade honestly.

use crate::mapping::ModelIdentity;
use std::collections::HashMap;
use std::collections::HashSet;

use codex_history::CompactedItem;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::models::LocalShellStatus;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AgentReasoningEvent;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ImageGenerationEndEvent;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::WebSearchEndEvent;
use codex_protocol::protocol::WorldStateItem;
use lhc::intake_stream::TurnOutcome;
use lhc::messages::BlockType;
use lhc::messages::MessageKind;
use lhc::messages::MessageRecord;
use lhc::shared_tech::view::SessionAssistantPart;
use lhc::shared_tech::view::SessionAssistantPartType;
use lhc::shared_tech::view::SessionThreadView;
use lhc::shared_tech::view::SessionThreadViewEntry;
use lhc::shared_tech::view::SessionThreadViewEntrySource;
use lhc::shared_tech::view::SessionThreadViewMessage;
use lhc::shared_tech::view::SessionThreadViewRuntimeEntry;
use lhc::turns::TurnRecord;
use lhc::turns::TurnStatus;
use serde_json::Map;
use serde_json::Value;

/// Documented capture gaps for the reverse map and display regeneration.
pub const CAPTURE_GAPS: &[&str] = &[
    "FunctionCall vs CustomToolCall: both forward-map to tool_call; reverse discriminates by recovered host ResponseItemId prefix (fc_→FunctionCall, ctc_→CustomToolCall). Outputs: fco_→FunctionCallOutput, ctco_→CustomToolCallOutput; call_id pairing carries kind when id missing. Unknown/unrepresentable id prefixes → id=None (provider remints) + gap_notes entry — never an invalid pairing (ctc_ on FunctionCall)",
    "FunctionCall.namespace / CustomToolCall.namespace: not stored on tool_call payload → always None",
    "Message.phase never stored → None; ResponseItemId recovered only from id-primary idempotency keys (synthetic: keys → None)",
    "Reasoning content[] vs summary[] vs encrypted_content: text + signature stored on assistant_thinking; encrypted_content re-emitted only when stored provider/model/api match live_identity (R2 host identity gate); mismatch or missing identity → encrypted_content None",
    "LocalShellCall.status / WebSearchCall.status / ToolSearchCall.execution+status: defaulted (Completed / completed / empty)",
    "ImageGenerationCall: paired call+result → one complete item; unpaired call → status=unknown result=\"\"",
    "ToolSearchOutput.tools JSON / status: parsed from tool_result content when possible; status from stored isError",
    "AgentMessage (inter-agent): forward folds to runtime_note text; reverse cannot restore author/recipient structure → runtime_note user Message without display twin (stored text, no view prefix)",
    "AdditionalTools / Compaction / ContextCompaction: runtime_note stored text only; CompactionTrigger never captured",
    "ContentItem InputImage/InputAudio: forward embeds [image:url]/[audio:url] in text; reverse leaves plain text",
    "TokenCount.rate_limits / model_context_window: not in provider_usage → None; cumulative total undercounts where pre-slice-A rows lack provider_usage; usage on rolled-back turns is excluded from the projected cumulative (those turns are out of the tail)",
    "TurnStarted.trace_id / model_context_window / collaboration_mode_kind: not in LHC turns → defaults; pre-boundary turns get no lifecycle events (post-boundary only)",
    "TurnAbortReason enum: coarse map from outcome_reason string; unknown → Interrupted",
    "ThreadSettingsApplied / ThreadGoalUpdated: not in LHC → carry-forward only",
    "ThreadRolledBack: applied by excluding dropped user turns from the regenerated tail via positional alignment of prior post-boundary user segments to LHC user-prompt turns (no marker emitted). Alignment mismatch falls back to under-exclusion (exclude nothing) with a gap_notes entry — never text-set membership, which over-excludes duplicate prompts. Rolled-back content already compressed into bands remains until the LHC rollback-capture batch lands",
    "Review / patch / MCP / subagent end-events: not in LHC → carry-forward only",
    "ItemCompleted(Plan|Sleep) / InterAgentCommunication{,Metadata}: not regenerable from LHC → carry-forward only",
    "TurnContextItem / previous_turn_settings: optional input (slice C wires post-boundary TurnContext). Without it, reconstruction leaves previous_turn_settings None and the reverse-scan early-exit at settings+context is disabled",
    "Pre-slice-A turns: outcome/timing/provider_usage absent → TurnComplete without timestamps; no TokenCount; cumulative totals undercount",
    "runtime_note shape: restored as user-role Message with stored payload text (no display twin); original host provenance (HostContext vs AgentMessage vs scaffolding) is lost",
    "Fork compact-marker runtime_notes (idempotency key namespace codex:{tid}:compact_marker:…): excluded from model and display streams — fork bookkeeping, not conversation; stay in the LHC record; boundary Compacted is the compact signal. Matched by key segment only (law 6), never note text",
    "synthetic: call_id strings retained only so call/result pairs still match; never promoted to ResponseItemId",
    "Display twin order: ResponseItem then EventMsg twin (live append is twin-then-item on some paths; reconstruction does not care)",
];

/// Window / compact metadata for the single boundary `Compacted` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactBoundaryMeta {
    pub message: String,
    pub window_number: u64,
    pub first_window_id: String,
    pub previous_window_id: Option<String>,
    pub window_id: String,
}

/// Inputs for [`materialize_rollout`]. Pure data — no handles, no paths.
#[derive(Debug, Clone)]
pub struct MaterializeInput<'a> {
    pub session_meta: SessionMetaLine,
    pub thread_view: &'a SessionThreadView,
    pub messages: &'a [MessageRecord],
    pub turns: &'a [TurnRecord],
    pub prior_generation: &'a [RolloutItem],
    pub boundary: CompactBoundaryMeta,
    pub world_state: Option<Value>,
    /// Optional post-boundary `TurnContext` for `previous_turn_settings` recovery.
    /// Slice C supplies the live session's latest context; `None` leaves the gap.
    pub turn_context: Option<TurnContextItem>,
    /// When set and matching the stored assistant provider/model/api, reverse
    /// maps restore `encrypted_content` from the thinking signature.
    pub live_identity: Option<ModelIdentity>,
}

/// Output of [`materialize_rollout`]: items plus any loud degradation notes.
#[derive(Debug, Clone)]
pub struct MaterializeResult {
    pub items: Vec<RolloutItem>,
    /// Non-empty when a capture/alignment gap forced under-exclusion or similar.
    pub gap_notes: Vec<String>,
}

/// Materialize a complete Legacy-mode rollout item sequence.
pub fn materialize_rollout(input: &MaterializeInput<'_>) -> MaterializeResult {
    let messages_by_id = index_messages(input.messages);
    let turns_by_id = index_turns(input.turns);
    let (rolled_back_turns, mut gap_notes) =
        rolled_back_turn_ids(input.prior_generation, input.messages, input.turns);
    // H2: rolled-back turns' usage must not inflate projected totals.
    let usage_totals = cumulative_usage_index(input.messages, &rolled_back_turns);

    let (band_entries, tail_entries) = split_band_and_tail(&input.thread_view.entries);

    let mut out: Vec<RolloutItem> = Vec::new();
    out.push(RolloutItem::SessionMeta(input.session_meta.clone()));

    push_carry_forwards_settings_goal(&mut out, input.prior_generation);

    // H5: first UserMessage from the true first user_prompt — display only.
    if let Some(first) = first_user_prompt_text(input.messages) {
        out.push(user_message_event(&first));
    }

    // Model stream = banded history as text (no display twins — H5).
    let mut model_stream: Vec<ResponseItem> = Vec::new();
    for entry in &band_entries {
        emit_band_entry(entry, &mut model_stream, &mut out);
    }

    let compacted = CompactedItem {
        message: input.boundary.message.clone(),
        replacement_history: Some(model_stream.iter().cloned().map(Into::into).collect()),
        mcp_resource_origins: None,
        window_number: Some(input.boundary.window_number),
        first_window_id: Some(input.boundary.first_window_id.clone()),
        previous_window_id: input.boundary.previous_window_id.clone(),
        window_id: Some(input.boundary.window_id.clone()),
    };
    debug_assert!(
        compacted
            .replacement_history
            .as_ref()
            .is_some_and(|h| !h.is_empty() || model_stream.is_empty())
            && compacted.window_number.is_some(),
        "boundary Compacted must carry replacement_history AND window_number"
    );
    out.push(RolloutItem::Compacted(compacted));

    if let Some(state) = input.world_state.clone() {
        match state {
            Value::Object(state) => {
                out.push(RolloutItem::WorldState(WorldStateItem::full(state)));
            }
            _ => gap_notes.push(
                "world_state snapshot was not an object; omitted from rebuilt rollout".to_string(),
            ),
        }
    }
    out.push(RolloutItem::EventMsg(EventMsg::ContextCompacted(
        ContextCompactedEvent {},
    )));

    if let Some(ctx) = input.turn_context.clone() {
        out.push(RolloutItem::TurnContext(ctx));
    }

    emit_tail(
        &tail_entries,
        &messages_by_id,
        &turns_by_id,
        input.turns,
        &usage_totals,
        &rolled_back_turns,
        input.live_identity.as_ref(),
        &mut out,
        &mut gap_notes,
    );

    // Carry-forward non-derivable ends (NOT ThreadRolledBack — C1).
    push_carry_forwards_ends(&mut out, input.prior_generation);

    if !gap_notes.is_empty() {
        for note in &gap_notes {
            tracing::warn!(%note, "LHC materialize gap");
        }
    }

    MaterializeResult {
        items: out,
        gap_notes: std::mem::take(&mut gap_notes),
    }
}

// ── indexing ──────────────────────────────────────────────────────────────

fn index_messages(messages: &[MessageRecord]) -> HashMap<String, &MessageRecord> {
    messages.iter().map(|m| (m.message_id.clone(), m)).collect()
}

fn index_turns(turns: &[TurnRecord]) -> HashMap<String, &TurnRecord> {
    turns.iter().map(|t| (t.turn_id.clone(), t)).collect()
}

/// Per-message cumulative token totals: `(total_up_to_including, last_call)`.
/// Messages belonging to `exclude_turns` (rollback-excluded) are omitted entirely.
fn cumulative_usage_index(
    messages: &[MessageRecord],
    exclude_turns: &HashSet<String>,
) -> HashMap<String, (TokenUsage, TokenUsage)> {
    let mut ordered: Vec<&MessageRecord> = messages
        .iter()
        .filter(|m| m.provider_usage.is_some() && !exclude_turns.contains(&m.turn_id))
        .collect();
    ordered.sort_by_key(|m| m.source_event_order);
    let mut cumulative = TokenUsage::default();
    let mut out = HashMap::new();
    for m in ordered {
        let Some(map) = m.provider_usage.as_ref() else {
            continue;
        };
        let Ok(last) = serde_json::from_value::<TokenUsage>(Value::Object(map.clone())) else {
            continue;
        };
        cumulative.add_assign(&last);
        out.insert(m.message_id.clone(), (cumulative.clone(), last));
    }
    out
}

fn first_user_prompt_text(messages: &[MessageRecord]) -> Option<String> {
    messages
        .iter()
        .filter(|m| m.kind == MessageKind::UserPrompt)
        .min_by_key(|m| m.source_event_order)
        .map(stored_text)
}

fn stored_text(m: &MessageRecord) -> String {
    m.blocks
        .iter()
        .filter_map(|b| b.content.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn stored_tool_is_error(m: &MessageRecord) -> Option<bool> {
    m.blocks
        .iter()
        .find(|b| b.block_type == BlockType::ToolResult)
        .and_then(|b| b.content.get("isError"))
        .and_then(Value::as_bool)
}

/// One post-boundary user-turn segment from the prior generation.
#[derive(Debug, Clone)]
struct PriorUserSegment {
    /// Chronologically first user-message text of the segment (alignment key).
    text: String,
    /// True when reverse-scan arithmetic would drop this segment.
    dropped: bool,
}

/// Post-boundary slice of `prior`: after the last `Compacted` with
/// `replacement_history`, or the whole file if none.
fn prior_post_boundary_slice(prior: &[RolloutItem]) -> &[RolloutItem] {
    let start = prior
        .iter()
        .rposition(|item| {
            matches!(
                item,
                RolloutItem::Compacted(c) if c.replacement_history.is_some()
            )
        })
        .map(|i| i + 1)
        .unwrap_or(0);
    &prior[start..]
}

/// Chronological (oldest→newest) post-boundary user segments with drop flags
/// from the same reverse-scan arithmetic as `rollout_reconstruction`.
fn prior_user_segments_with_drop(prior: &[RolloutItem]) -> Vec<PriorUserSegment> {
    let slice = prior_post_boundary_slice(prior);
    let mut pending = 0usize;
    let mut segments_newest_first: Vec<PriorUserSegment> = Vec::new();
    // Within a reverse segment, the last assignment is the chronologically first UM.
    let mut segment_text: Option<String> = None;
    let mut segment_is_user = false;

    let finalize = |pending: &mut usize,
                    text: &mut Option<String>,
                    is_user: &mut bool,
                    out: &mut Vec<PriorUserSegment>| {
        if *is_user {
            let dropped = if *pending > 0 {
                *pending = pending.saturating_sub(1);
                true
            } else {
                false
            };
            out.push(PriorUserSegment {
                text: text.take().unwrap_or_default(),
                dropped,
            });
        } else {
            *text = None;
        }
        *is_user = false;
    };

    for item in slice.iter().rev() {
        match item {
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(r)) => {
                pending =
                    pending.saturating_add(usize::try_from(r.num_turns).unwrap_or(usize::MAX));
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(u)) => {
                segment_is_user = true;
                // Overwrite: last write under reverse walk = chronologically first.
                segment_text = Some(u.message.clone());
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(_)) => {
                finalize(
                    &mut pending,
                    &mut segment_text,
                    &mut segment_is_user,
                    &mut segments_newest_first,
                );
            }
            RolloutItem::ResponseItem(item) => {
                if let ResponseItem::Message { role, content, .. } = &item.item
                    && role == "user"
                {
                    segment_is_user = true;
                    let t = content_text(content);
                    if !t.is_empty() {
                        segment_text = Some(t);
                    }
                }
            }
            _ => {}
        }
    }
    finalize(
        &mut pending,
        &mut segment_text,
        &mut segment_is_user,
        &mut segments_newest_first,
    );
    segments_newest_first.reverse();
    segments_newest_first
}

/// LHC user-prompt turns ordered by `turn_order` (stable secondary: event order).
fn lhc_user_prompt_turns_ordered(
    messages: &[MessageRecord],
    turns: &[TurnRecord],
) -> Vec<(String, String)> {
    let turn_order: HashMap<&str, i64> = turns
        .iter()
        .map(|t| (t.turn_id.as_str(), t.turn_order))
        .collect();
    let mut rows: Vec<(i64, i64, String, String)> = messages
        .iter()
        .filter(|m| m.kind == MessageKind::UserPrompt)
        .map(|m| {
            let order = turn_order
                .get(m.turn_id.as_str())
                .copied()
                .unwrap_or(i64::MAX);
            (
                order,
                m.source_event_order,
                m.turn_id.clone(),
                stored_text(m),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    // One row per turn_id (first user_prompt of the turn).
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (_, _, tid, text) in rows {
        if seen.insert(tid.clone()) {
            out.push((tid, text));
        }
    }
    out
}

/// Resolve prior rollback drops to LHC turn ids by **positional alignment**.
///
/// Prior post-boundary user segments (oldest→newest) align one-to-one with the
/// corresponding suffix of LHC user-prompt turns (by `turn_order`). Text is the
/// alignment check; position is the discriminator. On length or text mismatch:
/// exclude nothing and emit a gap note (under-exclusion preferred to destroying
/// live turns).
fn rolled_back_turn_ids(
    prior: &[RolloutItem],
    messages: &[MessageRecord],
    turns: &[TurnRecord],
) -> (HashSet<String>, Vec<String>) {
    let segments = prior_user_segments_with_drop(prior);
    if !segments.iter().any(|s| s.dropped) {
        return (HashSet::new(), Vec::new());
    }

    let lhc = lhc_user_prompt_turns_ordered(messages, turns);
    if lhc.len() < segments.len() {
        let note = format!(
            "rollback alignment: fewer LHC user-prompt turns ({}) than prior post-boundary segments ({}); excluding nothing (under-exclusion)",
            lhc.len(),
            segments.len()
        );
        return (HashSet::new(), vec![note]);
    }

    // Same range: suffix of LHC turns matching post-boundary segment count.
    let offset = lhc.len() - segments.len();
    let lhc_range = &lhc[offset..];

    for (i, (seg, (_, lhc_text))) in segments.iter().zip(lhc_range.iter()).enumerate() {
        if seg.text != *lhc_text {
            let note = format!(
                "rollback alignment mismatch at position {i}: prior_segment={:?} lhc_turn={:?}; excluding nothing (under-exclusion)",
                seg.text, lhc_text
            );
            return (HashSet::new(), vec![note]);
        }
    }

    let excluded = segments
        .iter()
        .zip(lhc_range.iter())
        .filter(|(seg, _)| seg.dropped)
        .map(|(_, (tid, _))| tid.clone())
        .collect();
    (excluded, Vec::new())
}

fn split_band_and_tail(
    entries: &[SessionThreadViewEntry],
) -> (Vec<&SessionThreadViewEntry>, Vec<&SessionThreadViewEntry>) {
    let mut bands = Vec::new();
    let mut tail = Vec::new();
    let mut seen_tail = false;
    for entry in entries {
        if !seen_tail && is_band_entry(entry) {
            bands.push(entry);
        } else {
            seen_tail = true;
            tail.push(entry);
        }
    }
    (bands, tail)
}

fn is_band_entry(entry: &SessionThreadViewEntry) -> bool {
    match entry {
        SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u)) => {
            u.source_messages.is_empty()
        }
        SessionThreadViewEntry::Runtime(_)
        | SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(_))
        | SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(_)) => false,
    }
}

// ── band → text model stream (no display twins) ───────────────────────────

fn emit_band_entry(
    entry: &SessionThreadViewEntry,
    model_stream: &mut Vec<ResponseItem>,
    out: &mut Vec<RolloutItem>,
) {
    let SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u)) = entry else {
        return;
    };
    if u.content.is_empty() {
        return;
    }
    let item = user_text_message(&u.content);
    model_stream.push(item.clone());
    out.push(RolloutItem::ResponseItem(item.into()));
}

fn user_text_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_text_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn user_message_event(text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        client_id: None,
        message: text.to_string(),
        images: None,
        image_details: Vec::new(),
        local_images: Vec::new(),
        local_image_details: Vec::new(),
        audio: None,
        local_audio: Vec::new(),
        text_elements: Vec::new(),
    }))
}

/// ResponseItem first, then display twin (L17).
fn push_response_with_twins(item: ResponseItem, out: &mut Vec<RolloutItem>, with_twins: bool) {
    out.push(RolloutItem::ResponseItem(item.clone().into()));
    if with_twins {
        emit_display_twins(&item, out);
    }
}

fn emit_display_twins(item: &ResponseItem, out: &mut Vec<RolloutItem>) {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let text = content_text(content);
            if text.is_empty() {
                return;
            }
            if role == "user" {
                out.push(user_message_event(&text));
            } else if role == "assistant" {
                out.push(RolloutItem::EventMsg(EventMsg::AgentMessage(
                    AgentMessageEvent {
                        message: text,
                        phase: None,
                        memory_citation: None,
                        delivery: None,
                    },
                )));
            }
        }
        ResponseItem::Reasoning { summary, .. } => {
            // encrypted_content re-emitted only on identity match (R2); else None.
            let summary_text = summary
                .iter()
                .map(|s| match s {
                    ReasoningItemReasoningSummary::SummaryText { text } => text.as_str(),
                })
                .collect::<Vec<_>>()
                .join("");
            if !summary_text.is_empty() {
                out.push(RolloutItem::EventMsg(EventMsg::AgentReasoning(
                    AgentReasoningEvent { text: summary_text },
                )));
            }
        }
        ResponseItem::WebSearchCall {
            id,
            action: Some(action),
            ..
        } => {
            let call_id = id
                .as_ref()
                .map(|i| i.as_str().to_string())
                .unwrap_or_default();
            let query = match action {
                WebSearchAction::Search { query, .. } => query.clone().unwrap_or_default(),
                WebSearchAction::OpenPage { url } => url.clone().unwrap_or_default(),
                WebSearchAction::FindInPage { url, .. } => url.clone().unwrap_or_default(),
                WebSearchAction::Other => String::new(),
            };
            out.push(RolloutItem::EventMsg(EventMsg::WebSearchEnd(
                WebSearchEndEvent {
                    call_id,
                    query,
                    action: action.clone(),
                    results: None,
                },
            )));
        }
        ResponseItem::ImageGenerationCall {
            id,
            status,
            revised_prompt,
            result,
            ..
        } => {
            let call_id = id
                .as_ref()
                .map(|i| i.as_str().to_string())
                .unwrap_or_default();
            out.push(RolloutItem::EventMsg(EventMsg::ImageGenerationEnd(
                ImageGenerationEndEvent {
                    call_id,
                    status: status.clone(),
                    revised_prompt: revised_prompt.clone(),
                    result: result.clone(),
                    failure: None,
                    transparent_background: None,
                    saved_path: None,
                },
            )));
        }
        _ => {}
    }
}

fn content_text(content: &[ContentItem]) -> String {
    content
        .iter()
        .map(|c| match c {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => text.as_str(),
            ContentItem::InputImage { image_url, .. } => image_url.as_str(),
            ContentItem::InputAudio { audio_url } => audio_url.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── tail reverse map ──────────────────────────────────────────────────────

struct PendingImage {
    revised_prompt: Option<String>,
    /// Owning turn — unpaired flush must land before that turn closes.
    turn_id: String,
}

/// Host ResponseItem kind recovered for a tool call (call_id → kind).
/// Used so tool_result can emit CustomToolCallOutput vs FunctionCallOutput
/// when the result row has no recoverable id of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveredToolCallKind {
    Function,
    Custom,
    LocalShell,
    Other,
}

fn emit_tail(
    tail: &[&SessionThreadViewEntry],
    messages_by_id: &HashMap<String, &MessageRecord>,
    turns_by_id: &HashMap<String, &TurnRecord>,
    all_turns: &[TurnRecord],
    usage_totals: &HashMap<String, (TokenUsage, TokenUsage)>,
    rolled_back_turns: &HashSet<String>,
    live_identity: Option<&ModelIdentity>,
    out: &mut Vec<RolloutItem>,
    gap_notes: &mut Vec<String>,
) {
    let mut opened: HashSet<String> = HashSet::new();
    let mut closed: HashSet<String> = HashSet::new();
    let mut pending_images: HashMap<String, PendingImage> = HashMap::new();
    let mut tool_call_kinds: HashMap<String, RecoveredToolCallKind> = HashMap::new();

    for entry in tail {
        // C1: skip entries belonging to rolled-back turns.
        if entry_belongs_to_rolled_back(entry, messages_by_id, rolled_back_turns) {
            continue;
        }

        // F1 / law 6: fork compact-marker runtime notes are bookkeeping (key
        // namespace codex:{tid}:compact_marker:…), not conversation. Exclude
        // from model stream and display stream before turn open / emit.
        // Matched on source idempotency key only — never note text.
        if entry_is_fork_compact_marker(entry) {
            continue;
        }

        for mid in entry_message_ids(entry) {
            if let Some(msg) = messages_by_id.get(&mid) {
                if rolled_back_turns.contains(&msg.turn_id) {
                    continue;
                }
                maybe_open_turn(
                    msg.turn_id.as_str(),
                    turns_by_id,
                    &mut opened,
                    &mut closed,
                    &mut pending_images,
                    out,
                );
            }
        }

        match entry {
            SessionThreadViewEntry::Runtime(_) => {}
            SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u)) => {
                // M6: classify by source-message kind, never view text.
                let is_runtime_note = u.source_messages.iter().any(|s| {
                    messages_by_id
                        .get(&s.message_id)
                        .is_some_and(|m| m.kind == MessageKind::RuntimeNote)
                });
                if is_runtime_note {
                    let text = u
                        .source_messages
                        .iter()
                        .find_map(|s| {
                            messages_by_id.get(&s.message_id).and_then(|m| {
                                if m.kind == MessageKind::RuntimeNote {
                                    Some(stored_text(m))
                                } else {
                                    None
                                }
                            })
                        })
                        .unwrap_or_else(|| u.content.clone());
                    if text.is_empty() {
                        continue;
                    }
                    push_response_with_twins(
                        user_text_message(&text),
                        out,
                        /*with_twins*/ false,
                    );
                } else {
                    if u.content.is_empty() {
                        continue;
                    }
                    let text = u
                        .source_messages
                        .first()
                        .and_then(|s| messages_by_id.get(&s.message_id))
                        .map(|m| stored_text(m))
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| u.content.clone());
                    push_response_with_twins(user_text_message(&text), out, true);
                }
            }
            SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(a)) => {
                for (idx, part) in a.content.iter().enumerate() {
                    let source = a.source_messages.get(idx);
                    let msg = source
                        .and_then(|s| messages_by_id.get(&s.message_id))
                        .copied();
                    if msg.is_some_and(|m| rolled_back_turns.contains(&m.turn_id)) {
                        continue;
                    }
                    let id_hint = source.and_then(|s| {
                        s.idempotency_key
                            .as_deref()
                            .and_then(parse_host_id_from_key)
                    });
                    let turn_id = msg.map(|m| m.turn_id.as_str());
                    let stored_identity = ModelIdentity {
                        provider: a.provider.clone(),
                        model: a.model.clone(),
                        api: a.api.clone(),
                    };
                    reverse_assistant_part(
                        part,
                        msg,
                        id_hint.as_deref(),
                        turn_id,
                        &stored_identity,
                        live_identity,
                        &mut pending_images,
                        &mut tool_call_kinds,
                        &mut *gap_notes,
                        out,
                    );
                    // H2: emit TokenCount after the LAST part of a usage-bearing
                    // message, regardless of part kind (text/thinking/tool).
                    let cur_mid = source.map(|s| s.message_id.as_str());
                    let next_mid = a
                        .source_messages
                        .get(idx + 1)
                        .map(|s| s.message_id.as_str());
                    if cur_mid != next_mid
                        && let Some(m) = msg
                        && let Some((total, last)) = usage_totals.get(&m.message_id)
                    {
                        out.push(RolloutItem::EventMsg(EventMsg::TokenCount(
                            TokenCountEvent {
                                info: Some(TokenUsageInfo {
                                    total_token_usage: total.clone(),
                                    last_token_usage: last.clone(),
                                    model_context_window: None,
                                }),
                                rate_limits: None,
                            },
                        )));
                    }
                }
            }
            SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(tr)) => {
                let source = tr.source_messages.first();
                let msg = source
                    .and_then(|s| messages_by_id.get(&s.message_id))
                    .copied();
                if msg.is_some_and(|m| rolled_back_turns.contains(&m.turn_id)) {
                    continue;
                }
                // Law 6: route by tool identity only — never content sniffing.
                let is_image = tr.tool_name.as_deref() == Some("image_generation")
                    || pending_images.contains_key(&tr.tool_call_id);
                if is_image {
                    let pending = pending_images.remove(&tr.tool_call_id);
                    let item = complete_image_generation(
                        &tr.tool_call_id,
                        &tr.content,
                        pending.as_ref(),
                        msg,
                    );
                    push_response_with_twins(item, out, true);
                } else {
                    let is_error = msg.and_then(stored_tool_is_error).or(tr.is_error);
                    let id_hint = source.and_then(|s| {
                        s.idempotency_key
                            .as_deref()
                            .and_then(parse_host_id_from_key)
                    });
                    let call_kind = tool_call_kinds.get(tr.tool_call_id.as_str()).copied();
                    let item = reverse_tool_result(
                        &tr.tool_call_id,
                        tr.tool_name.as_deref(),
                        &tr.content,
                        is_error,
                        id_hint.as_deref(),
                        call_kind,
                        &mut *gap_notes,
                    );
                    push_response_with_twins(item, out, true);
                }
                // H2: tool_result messages can carry usage (rare); emit if last of that msg.
                if let Some(m) = msg
                    && let Some((total, last)) = usage_totals.get(&m.message_id)
                {
                    out.push(RolloutItem::EventMsg(EventMsg::TokenCount(
                        TokenCountEvent {
                            info: Some(TokenUsageInfo {
                                total_token_usage: total.clone(),
                                last_token_usage: last.clone(),
                                model_context_window: None,
                            }),
                            rate_limits: None,
                        },
                    )));
                }
            }
        }

        for mid in entry_message_ids(entry) {
            if let Some(msg) = messages_by_id.get(&mid) {
                if rolled_back_turns.contains(&msg.turn_id) {
                    continue;
                }
                maybe_close_turn_if_last(
                    msg,
                    turns_by_id,
                    &mut closed,
                    &opened,
                    &mut pending_images,
                    out,
                );
            }
        }
    }

    // Remaining unpaired images (open turns that never closed): flush now.
    flush_all_pending_images(&mut pending_images, out);

    // H4: end sweep handles only Closed turns still open (typically the final
    // turn). Closes for earlier turns happen at turn-boundary transitions.
    let mut remaining: Vec<&TurnRecord> = all_turns
        .iter()
        .filter(|t| {
            t.status == TurnStatus::Closed
                && opened.contains(&t.turn_id)
                && !closed.contains(&t.turn_id)
                && !rolled_back_turns.contains(&t.turn_id)
        })
        .collect();
    remaining.sort_by_key(|t| t.turn_order);
    for turn in remaining {
        flush_pending_images_for_turn(&turn.turn_id, &mut pending_images, out);
        emit_turn_end(turn, out);
        closed.insert(turn.turn_id.clone());
    }
}

fn flush_pending_images_for_turn(
    turn_id: &str,
    pending: &mut HashMap<String, PendingImage>,
    out: &mut Vec<RolloutItem>,
) {
    let call_ids: Vec<String> = pending
        .iter()
        .filter(|(_, p)| p.turn_id == turn_id)
        .map(|(id, _)| id.clone())
        .collect();
    for call_id in call_ids {
        if let Some(p) = pending.remove(&call_id) {
            let item = ResponseItem::ImageGenerationCall {
                id: response_item_id_opt(&call_id),
                status: "unknown".into(),
                revised_prompt: p.revised_prompt,
                result: String::new(),
                internal_chat_message_metadata_passthrough: None,
            };
            push_response_with_twins(item, out, true);
        }
    }
}

fn flush_all_pending_images(
    pending: &mut HashMap<String, PendingImage>,
    out: &mut Vec<RolloutItem>,
) {
    let call_ids: Vec<String> = pending.keys().cloned().collect();
    for call_id in call_ids {
        if let Some(p) = pending.remove(&call_id) {
            let item = ResponseItem::ImageGenerationCall {
                id: response_item_id_opt(&call_id),
                status: "unknown".into(),
                revised_prompt: p.revised_prompt,
                result: String::new(),
                internal_chat_message_metadata_passthrough: None,
            };
            push_response_with_twins(item, out, true);
        }
    }
}

/// True when this view entry is sourced from a fork compact-marker runtime note
/// (idempotency key in the `codex:{tid}:compact_marker:…` namespace).
///
/// Structural key match via the view's source join — never note body text.
fn entry_is_fork_compact_marker(entry: &SessionThreadViewEntry) -> bool {
    entry_source_keys(entry).any(crate::is_compact_marker_idempotency_key)
}

fn entry_source_keys(entry: &SessionThreadViewEntry) -> impl Iterator<Item = &str> {
    let sources: &[SessionThreadViewEntrySource] = match entry {
        SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u)) => &u.source_messages,
        SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(a)) => {
            &a.source_messages
        }
        SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(tr)) => {
            &tr.source_messages
        }
        SessionThreadViewEntry::Runtime(SessionThreadViewRuntimeEntry::ModelChange(m)) => {
            &m.source_messages
        }
        SessionThreadViewEntry::Runtime(SessionThreadViewRuntimeEntry::ThinkingLevelChange(t)) => {
            &t.source_messages
        }
    };
    sources.iter().filter_map(|s| s.idempotency_key.as_deref())
}

fn entry_belongs_to_rolled_back(
    entry: &SessionThreadViewEntry,
    messages_by_id: &HashMap<String, &MessageRecord>,
    rolled_back: &HashSet<String>,
) -> bool {
    if rolled_back.is_empty() {
        return false;
    }
    let ids = entry_message_ids(entry);
    if ids.is_empty() {
        return false;
    }
    // Skip when every sourced message belongs to a rolled-back turn.
    ids.iter().all(|mid| {
        messages_by_id
            .get(mid)
            .is_some_and(|m| rolled_back.contains(&m.turn_id))
    })
}

fn entry_message_ids(entry: &SessionThreadViewEntry) -> Vec<String> {
    match entry {
        SessionThreadViewEntry::Message(SessionThreadViewMessage::User(u)) => u
            .source_messages
            .iter()
            .map(|s| s.message_id.clone())
            .collect(),
        SessionThreadViewEntry::Message(SessionThreadViewMessage::Assistant(a)) => a
            .source_messages
            .iter()
            .map(|s| s.message_id.clone())
            .collect(),
        SessionThreadViewEntry::Message(SessionThreadViewMessage::ToolResult(tr)) => tr
            .source_messages
            .iter()
            .map(|s| s.message_id.clone())
            .collect(),
        SessionThreadViewEntry::Runtime(SessionThreadViewRuntimeEntry::ModelChange(m)) => m
            .source_messages
            .iter()
            .map(|s| s.message_id.clone())
            .collect(),
        SessionThreadViewEntry::Runtime(SessionThreadViewRuntimeEntry::ThinkingLevelChange(t)) => t
            .source_messages
            .iter()
            .map(|s| s.message_id.clone())
            .collect(),
    }
}

fn maybe_open_turn(
    turn_id: &str,
    turns_by_id: &HashMap<String, &TurnRecord>,
    opened: &mut HashSet<String>,
    closed: &mut HashSet<String>,
    pending_images: &mut HashMap<String, PendingImage>,
    out: &mut Vec<RolloutItem>,
) {
    if opened.contains(turn_id) {
        return;
    }
    // H4: before opening turn N+1, close any earlier Closed turns still open
    // so TurnComplete precedes the next TurnStarted (reconstruction segments).
    let new_order = turns_by_id
        .get(turn_id)
        .map(|t| t.turn_order)
        .unwrap_or(i64::MAX);
    let mut to_close: Vec<String> = opened
        .iter()
        .filter(|id| !closed.contains(*id))
        .filter(|id| {
            turns_by_id
                .get(id.as_str())
                .is_some_and(|t| t.status == TurnStatus::Closed && t.turn_order < new_order)
        })
        .cloned()
        .collect();
    to_close.sort_by_key(|id| {
        turns_by_id
            .get(id.as_str())
            .map(|t| t.turn_order)
            .unwrap_or(0)
    });
    for id in to_close {
        if let Some(turn) = turns_by_id.get(id.as_str()) {
            flush_pending_images_for_turn(&id, pending_images, out);
            emit_turn_end(turn, out);
            closed.insert(id);
        }
    }

    opened.insert(turn_id.to_string());
    let turn = turns_by_id.get(turn_id);
    let started_at = turn.and_then(|t| t.started_at.as_deref().and_then(iso_to_unix_secs));
    out.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
        TurnStartedEvent {
            turn_id: turn_id.to_string(),
            trace_id: None,
            started_at,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        },
    )));
}

fn maybe_close_turn_if_last(
    msg: &MessageRecord,
    turns_by_id: &HashMap<String, &TurnRecord>,
    closed: &mut HashSet<String>,
    opened: &HashSet<String>,
    pending_images: &mut HashMap<String, PendingImage>,
    out: &mut Vec<RolloutItem>,
) {
    let Some(turn) = turns_by_id.get(&msg.turn_id) else {
        return;
    };
    if closed.contains(&turn.turn_id) || !opened.contains(&turn.turn_id) {
        return;
    }
    let is_last = turn
        .member_message_ids
        .last()
        .is_some_and(|last| last == &msg.message_id);
    if !is_last {
        return;
    }
    // H4: open turns stay open — no fabricated TurnComplete.
    if turn.status != TurnStatus::Closed {
        return;
    }
    flush_pending_images_for_turn(&turn.turn_id, pending_images, out);
    emit_turn_end(turn, out);
    closed.insert(turn.turn_id.clone());
}

fn emit_turn_end(turn: &TurnRecord, out: &mut Vec<RolloutItem>) {
    let started_at = turn.started_at.as_deref().and_then(iso_to_unix_secs);
    let ended_at = turn.ended_at.as_deref().and_then(iso_to_unix_secs);
    let duration_ms = match (started_at, ended_at) {
        (Some(s), Some(e)) if e >= s => Some((e - s).saturating_mul(1000)),
        _ => None,
    };

    match turn.outcome {
        Some(TurnOutcome::Aborted) => {
            let reason = map_abort_reason(turn.outcome_reason.as_deref());
            out.push(RolloutItem::EventMsg(EventMsg::TurnAborted(
                TurnAbortedEvent {
                    turn_id: Some(turn.turn_id.clone()),
                    reason,
                    started_at,
                    completed_at: ended_at,
                    duration_ms,
                },
            )));
        }
        Some(TurnOutcome::Completed) | None => {
            out.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
                TurnCompleteEvent {
                    turn_id: turn.turn_id.clone(),
                    last_agent_message: None,
                    error: None,
                    started_at,
                    completed_at: ended_at,
                    duration_ms,
                    time_to_first_token_ms: None,
                },
            )));
        }
    }
}

fn map_abort_reason(reason: Option<&str>) -> TurnAbortReason {
    let Some(raw) = reason else {
        return TurnAbortReason::Interrupted;
    };
    let s = raw.to_ascii_lowercase();
    if s.contains("replac") {
        TurnAbortReason::Replaced
    } else if s.contains("review") {
        TurnAbortReason::ReviewEnded
    } else if s.contains("budget") {
        TurnAbortReason::BudgetLimited
    } else {
        TurnAbortReason::Interrupted
    }
}

fn reverse_assistant_part(
    part: &SessionAssistantPart,
    msg: Option<&MessageRecord>,
    id_hint: Option<&str>,
    turn_id: Option<&str>,
    stored_identity: &ModelIdentity,
    live_identity: Option<&ModelIdentity>,
    pending_images: &mut HashMap<String, PendingImage>,
    tool_call_kinds: &mut HashMap<String, RecoveredToolCallKind>,
    gap_notes: &mut Vec<String>,
    out: &mut Vec<RolloutItem>,
) {
    match part.type_ {
        SessionAssistantPartType::Text => {
            let text = part.text.as_deref().unwrap_or("");
            if text.is_empty() {
                return;
            }
            let mut item = assistant_text_message(text);
            if let Some(id) = id_hint.and_then(sanitize_id) {
                item.set_id(Some(ResponseItemId::from_server(id)));
            }
            push_response_with_twins(item, out, true);
            // TokenCount deferred to caller after last part of this message.
            let _ = msg;
        }
        SessionAssistantPartType::Thinking => {
            let text = part.thinking.as_deref().unwrap_or("");
            let signature = part.thinking_signature.as_deref().filter(|s| !s.is_empty());
            // Signature-only thinking is not a husk — still emit for identity-gated
            // encrypted_content re-emit (R2). Empty text + no signature is a husk.
            if text.is_empty() && signature.is_none() {
                return;
            }
            // Identity-match: re-emit encrypted_content only when stored capture
            // identity matches the live session model. Missing either side → None.
            let encrypted_content = match (signature, live_identity) {
                (Some(sig), Some(live))
                    if stored_identity.is_complete() && stored_identity.matches(live) =>
                {
                    Some(sig.to_string())
                }
                _ => None,
            };
            let summary = if text.is_empty() {
                Vec::new()
            } else {
                vec![ReasoningItemReasoningSummary::SummaryText {
                    text: text.to_string(),
                }]
            };
            let item = ResponseItem::Reasoning {
                id: id_hint
                    .and_then(sanitize_id)
                    .map(ResponseItemId::from_server),
                summary,
                content: None,
                encrypted_content,
                internal_chat_message_metadata_passthrough: None,
            };
            push_response_with_twins(item, out, true);
        }
        SessionAssistantPartType::ToolCall => {
            let tool_call_id = part.tool_call_id.as_deref().unwrap_or("");
            let tool_name = part.tool_name.as_deref().unwrap_or("tool");
            let arguments = part.arguments.clone().unwrap_or_default();
            if tool_name == "image_generation" {
                // H3: defer until tool_result pairs; unpaired flushed before turn close.
                let revised = arguments
                    .get("revisedPrompt")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                pending_images.insert(
                    tool_call_id.to_string(),
                    PendingImage {
                        revised_prompt: revised,
                        turn_id: turn_id.unwrap_or("").to_string(),
                    },
                );
                return;
            }
            let (item, kind) =
                reverse_tool_call(tool_name, tool_call_id, &arguments, id_hint, gap_notes);
            if !tool_call_id.is_empty() {
                tool_call_kinds.insert(tool_call_id.to_string(), kind);
            }
            push_response_with_twins(item, out, true);
        }
    }
}

fn complete_image_generation(
    tool_call_id: &str,
    content: &str,
    pending: Option<&PendingImage>,
    msg: Option<&MessageRecord>,
) -> ResponseItem {
    let is_error = msg.and_then(stored_tool_is_error);
    if let Ok(v) = serde_json::from_str::<Value>(content) {
        let status = v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or(if is_error == Some(true) {
                "failed"
            } else {
                "completed"
            })
            .to_string();
        let result = v
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let revised = v
            .get("revisedPrompt")
            .and_then(|r| r.as_str())
            .map(str::to_string)
            .or_else(|| pending.and_then(|p| p.revised_prompt.clone()));
        return ResponseItem::ImageGenerationCall {
            id: response_item_id_opt(tool_call_id),
            status,
            revised_prompt: revised,
            result,
            internal_chat_message_metadata_passthrough: None,
        };
    }
    ResponseItem::ImageGenerationCall {
        id: response_item_id_opt(tool_call_id),
        status: if is_error == Some(true) {
            "failed".into()
        } else {
            "completed".into()
        },
        revised_prompt: pending.and_then(|p| p.revised_prompt.clone()),
        result: content.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

/// Host ResponseItemId kind prefix (structural identity metadata).
/// Longest-first so `ctco`/`fco` win over `ctc`/`fc`.
fn host_id_kind_prefix(id: &str) -> Option<&'static str> {
    const PREFIXES: &[&str] = &[
        "ctco", "ctc", "fco", "fc", "lsh", "tsc", "tso", "ws", "ig", "msg", "rs", "amsg", "at",
        "cmp",
    ];
    for p in PREFIXES {
        if id == *p || id.starts_with(&format!("{p}_")) {
            return Some(*p);
        }
    }
    None
}

fn host_raw_input(arguments: &Map<String, Value>) -> String {
    arguments
        .get("__hostRaw")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let mut a = arguments.clone();
            a.remove("__hostRaw");
            a.remove("__hostEncryptedFunctionArgs");
            serde_json::to_string(&Value::Object(a)).unwrap_or_else(|_| "{}".into())
        })
}

fn host_encrypted_function_args(arguments: &Map<String, Value>) -> Option<Vec<String>> {
    arguments
        .get("__hostEncryptedFunctionArgs")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
}

fn reverse_tool_call(
    tool_name: &str,
    tool_call_id: &str,
    arguments: &Map<String, Value>,
    id_hint: Option<&str>,
    gap_notes: &mut Vec<String>,
) -> (ResponseItem, RecoveredToolCallKind) {
    let recovered = id_hint.and_then(sanitize_id);
    let prefix = recovered.as_deref().and_then(host_id_kind_prefix);
    let call_id = call_id_string(tool_call_id);

    // Name-based specialized tools take precedence when capture recorded them.
    match tool_name {
        "local_shell" => {
            let mut args = arguments.clone();
            args.remove("__hostRaw");
            if !args.contains_key("type") {
                args.insert("type".into(), Value::String("exec".into()));
            }
            let action = serde_json::from_value::<LocalShellAction>(Value::Object(args)).unwrap_or(
                LocalShellAction::Exec(LocalShellExecAction {
                    command: Vec::new(),
                    timeout_ms: None,
                    working_directory: None,
                    env: None,
                    user: None,
                }),
            );
            let id = match prefix {
                None | Some("lsh") => recovered.map(ResponseItemId::from_server),
                Some(other) => {
                    gap_notes.push(format!(
                        "tool_call local_shell id prefix {other:?} unrepresentable with LocalShellCall; clearing id"
                    ));
                    None
                }
            };
            return (
                ResponseItem::LocalShellCall {
                    id,
                    call_id: non_empty_opt(&call_id),
                    status: LocalShellStatus::Completed,
                    action,
                    internal_chat_message_metadata_passthrough: None,
                },
                RecoveredToolCallKind::LocalShell,
            );
        }
        "web_search" => {
            let mut args = arguments.clone();
            args.remove("__hostRaw");
            let action = serde_json::from_value::<WebSearchAction>(Value::Object(args)).ok();
            let id = match prefix {
                None | Some("ws") => recovered
                    .as_deref()
                    .and_then(sanitize_id)
                    .map(ResponseItemId::from_server)
                    .or_else(|| response_item_id_opt(tool_call_id)),
                Some(other) => {
                    gap_notes.push(format!(
                        "tool_call web_search id prefix {other:?} unrepresentable with WebSearchCall; clearing id"
                    ));
                    None
                }
            };
            return (
                ResponseItem::WebSearchCall {
                    id,
                    status: Some("completed".into()),
                    action,
                    internal_chat_message_metadata_passthrough: None,
                },
                RecoveredToolCallKind::Other,
            );
        }
        "tool_search" => {
            let mut args = arguments.clone();
            let host_raw = args.remove("__hostRaw");
            let arguments_value = host_raw
                .and_then(|v| {
                    v.as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                })
                .unwrap_or(Value::Object(args));
            let id = match prefix {
                None | Some("tsc") => recovered.map(ResponseItemId::from_server),
                Some(other) => {
                    gap_notes.push(format!(
                        "tool_call tool_search id prefix {other:?} unrepresentable with ToolSearchCall; clearing id"
                    ));
                    None
                }
            };
            return (
                ResponseItem::ToolSearchCall {
                    id,
                    call_id: non_empty_opt(&call_id),
                    status: Some("completed".into()),
                    execution: String::new(),
                    arguments: arguments_value,
                    internal_chat_message_metadata_passthrough: None,
                },
                RecoveredToolCallKind::Other,
            );
        }
        _ => {}
    }

    // Generic tools: id prefix is the FunctionCall vs CustomToolCall discriminator.
    let raw = host_raw_input(arguments);
    match prefix {
        Some("ctc") => (
            ResponseItem::CustomToolCall {
                id: recovered.map(ResponseItemId::from_server),
                status: None,
                call_id,
                name: tool_name.to_string(),
                namespace: None,
                input: raw,
                internal_chat_message_metadata_passthrough: None,
            },
            RecoveredToolCallKind::Custom,
        ),
        Some("fc") | None => (
            ResponseItem::FunctionCall {
                id: recovered.map(ResponseItemId::from_server),
                name: tool_name.to_string(),
                namespace: None,
                arguments: raw,
                encrypted_function_args: host_encrypted_function_args(arguments),
                call_id,
                internal_chat_message_metadata_passthrough: None,
            },
            RecoveredToolCallKind::Function,
        ),
        Some(other) => {
            // Unrepresentable pairing (e.g. msg_/rs_ on a tool_call) — never
            // emit FunctionCall wearing a non-fc id.
            gap_notes.push(format!(
                "tool_call id prefix {other:?} unrepresentable as FunctionCall/CustomToolCall; id=None"
            ));
            (
                ResponseItem::FunctionCall {
                    id: None,
                    name: tool_name.to_string(),
                    namespace: None,
                    arguments: raw,
                    encrypted_function_args: host_encrypted_function_args(arguments),
                    call_id,
                    internal_chat_message_metadata_passthrough: None,
                },
                RecoveredToolCallKind::Function,
            )
        }
    }
}

fn reverse_tool_result(
    tool_call_id: &str,
    tool_name: Option<&str>,
    content: &str,
    is_error: Option<bool>,
    id_hint: Option<&str>,
    call_kind: Option<RecoveredToolCallKind>,
    gap_notes: &mut Vec<String>,
) -> ResponseItem {
    let name = tool_name.unwrap_or("");
    let call_id = call_id_string(tool_call_id);
    let recovered = id_hint.and_then(sanitize_id);
    let prefix = recovered.as_deref().and_then(host_id_kind_prefix);

    if name == "tool_search" {
        let tools = serde_json::from_str::<Vec<Value>>(content).unwrap_or_default();
        let status = if is_error == Some(true) {
            "failed"
        } else {
            "completed"
        };
        let id = match prefix {
            None | Some("tso") => recovered.map(ResponseItemId::from_server),
            Some(other) => {
                gap_notes.push(format!(
                    "tool_result tool_search id prefix {other:?} unrepresentable with ToolSearchOutput; clearing id"
                ));
                None
            }
        };
        return ResponseItem::ToolSearchOutput {
            id,
            call_id: non_empty_opt(&call_id),
            status: status.into(),
            execution: String::new(),
            tools,
            internal_chat_message_metadata_passthrough: None,
        };
    }

    // M8: success reversed from stored isError (Some(false) → Some(true)).
    let success = is_error.map(|e| !e);
    let payload = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text(content.to_string()),
        success,
    };

    // Prefer id prefix; fall back to paired call kind; default FunctionCallOutput.
    let want_custom = match prefix {
        Some("ctco") => true,
        Some("fco") => false,
        Some(other) => {
            gap_notes.push(format!(
                "tool_result id prefix {other:?} unrepresentable as fco/ctco; id=None, kind from call pairing"
            ));
            matches!(call_kind, Some(RecoveredToolCallKind::Custom))
        }
        None => matches!(call_kind, Some(RecoveredToolCallKind::Custom)),
    };
    let id = match prefix {
        Some("ctco") | Some("fco") => recovered.map(ResponseItemId::from_server),
        _ => None,
    };

    if want_custom {
        ResponseItem::CustomToolCallOutput {
            id,
            call_id,
            name: if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            },
            output: payload,
            internal_chat_message_metadata_passthrough: None,
        }
    } else {
        ResponseItem::FunctionCallOutput {
            id,
            call_id: non_empty_opt(&call_id),
            name: None,
            namespace: None,
            output: payload,
            internal_chat_message_metadata_passthrough: None,
        }
    }
}

/// M10: `synthetic:` and empty → not a real id.
fn sanitize_id(id: &str) -> Option<String> {
    if id.is_empty() || id.starts_with("synthetic:") {
        None
    } else {
        Some(id.to_string())
    }
}

fn response_item_id_opt(id: &str) -> Option<ResponseItemId> {
    sanitize_id(id).map(ResponseItemId::from_server)
}

fn call_id_string(id: &str) -> String {
    // Retain synthetic strings only so call/result pairs still match (gap).
    id.to_string()
}

fn non_empty_opt(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Inverse of [`crate::mapping::unix_secs_to_iso`] (and tolerant of bare ints).
pub fn iso_to_unix_secs(iso: &str) -> Option<i64> {
    if let Ok(n) = iso.parse::<i64>() {
        return Some(n);
    }
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.fZ")
                .ok()
                .map(|ndt| ndt.and_utc().timestamp())
        })
}

/// Parse host ResponseItemId from a codex id-primary idempotency key.
fn parse_host_id_from_key(key: &str) -> Option<String> {
    let rest = key.strip_prefix("codex:")?;
    let after_tid = rest.split_once(':')?.1;
    let after_id = after_tid.strip_prefix("id:")?;
    let iid = after_id.split(':').next()?;
    let decoded = decode_percent(iid);
    sanitize_id(&decoded)
}

fn decode_percent(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&encoded[i + 1..i + 3], 16)
        {
            // UTF-8 safe: collect contiguous percent-decoded bytes.
            let mut raw = vec![v];
            i += 3;
            while i + 2 < bytes.len() && bytes[i] == b'%' {
                if let Ok(b) = u8::from_str_radix(&encoded[i + 1..i + 3], 16) {
                    raw.push(b);
                    i += 3;
                } else {
                    break;
                }
            }
            match String::from_utf8(raw) {
                Ok(s) => out.push_str(&s),
                Err(e) => {
                    for b in e.into_bytes() {
                        out.push(b as char);
                    }
                }
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

// ── carry-forwards ────────────────────────────────────────────────────────

fn push_carry_forwards_settings_goal(out: &mut Vec<RolloutItem>, prior: &[RolloutItem]) {
    for item in prior {
        match item {
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(_))
            | RolloutItem::EventMsg(EventMsg::ThreadGoalUpdated(_)) => {
                out.push(item.clone());
            }
            _ => {}
        }
    }
}

fn push_carry_forwards_ends(out: &mut Vec<RolloutItem>, prior: &[RolloutItem]) {
    for item in prior {
        match item {
            // C1: ThreadRolledBack is applied by exclusion — never carried.
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)) => {}
            RolloutItem::EventMsg(EventMsg::EnteredReviewMode(_))
            | RolloutItem::EventMsg(EventMsg::ExitedReviewMode(_))
            | RolloutItem::EventMsg(EventMsg::PatchApplyEnd(_))
            | RolloutItem::EventMsg(EventMsg::McpToolCallEnd(_))
            | RolloutItem::EventMsg(EventMsg::SubAgentActivity(_)) => {
                out.push(item.clone());
            }
            // M7: plan/sleep ItemCompleted + inter-agent.
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ev))
                if matches!(&ev.item, TurnItem::Plan(_) | TurnItem::Extension(_)) =>
            {
                out.push(item.clone());
            }
            RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. } => {
                out.push(item.clone());
            }
            _ => {}
        }
    }
}

/// Inspect boundary completeness of a materialized sequence.
///
/// Requires exactly one `Compacted` with non-empty `replacement_history` and
/// `window_number` set.
pub fn boundary_completeness_error(items: &[RolloutItem]) -> Option<&'static str> {
    let compacted: Vec<&CompactedItem> = items
        .iter()
        .filter_map(|i| match i {
            RolloutItem::Compacted(c) => Some(c),
            _ => None,
        })
        .collect();
    if compacted.is_empty() {
        return Some("missing Compacted boundary record");
    }
    if compacted.len() > 1 {
        return Some("expected exactly one Compacted boundary record");
    }
    let c = compacted[0];
    match &c.replacement_history {
        None => return Some("Compacted.replacement_history is None"),
        Some(h) if h.is_empty() => {
            return Some("Compacted.replacement_history is empty");
        }
        Some(_) => {}
    }
    if c.window_number.is_none() {
        return Some("Compacted.window_number is None");
    }
    None
}

/// Count ResponseItems in the banded model stream (pre-Compacted).
pub fn model_stream_response_item_count(items: &[RolloutItem]) -> usize {
    let mut n = 0;
    for item in items {
        match item {
            RolloutItem::Compacted(_) => break,
            RolloutItem::ResponseItem(_) => n += 1,
            _ => {}
        }
    }
    n
}

#[cfg(test)]
#[path = "materialize_tests.rs"]
mod tests;
