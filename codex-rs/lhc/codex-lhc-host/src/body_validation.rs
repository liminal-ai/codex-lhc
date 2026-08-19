//! LIM-67: host full-body validation of the exact next provider request after
//! a protected-escalation core install.
//!
//! Core installation (LHC view + visibility boundary) and host validation are
//! intentionally separate states. This module validates the **materialized
//! item sequence Codex would actually send next** — provider structure, tool
//! correlation and ordering, protected call/result byte-stability, reasoning
//! survival, and the host safe-runway threshold — before any rollout rewrite,
//! in-memory replacement, or provider send. It never mutates durable state;
//! the caller records `ok`/`failed` through the certified SDK API.

use codex_protocol::models::ResponseItem;
use serde_json::Value;

/// Source label for the host body-size measurement (o200k estimate over the
/// serialized materialized item sequence).
pub const BODY_SIZE_SOURCE: &str = "codex_materialized_body_o200k_estimate";

/// Expected byte-stable content captured from the live pre-attempt history.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtectedPairExpectation {
    pub call_id: String,
    /// Normalized serialization (top-level `id` stripped) of the live call item.
    pub call_bytes: String,
    /// Normalized serialization (top-level `id` stripped) of the live output item.
    pub output_bytes: String,
}

/// Validation inputs for one attempt's exact materialized body.
#[derive(Debug, Clone)]
pub struct BodyValidationSpec {
    pub attempt_id: String,
    /// Sorted unique protected response-scoped call IDs.
    pub protected_tool_call_ids: Vec<String>,
    /// Live-history byte expectations for every protected pair.
    pub protected_pairs: Vec<ProtectedPairExpectation>,
    /// Encrypted reasoning payloads (`encrypted_content`) of live reasoning
    /// items in the current model window. These provider-signed bytes must
    /// survive verbatim; the certified materializer's canonical text shape
    /// (summary/content flattening, matching resume-from-file) may differ.
    pub required_encrypted_reasoning: Vec<String>,
    /// Host safe-runway threshold the complete body must stay strictly under.
    pub safe_runway_threshold_tokens: Option<i64>,
}

/// Successful validation report (inspectable receipt material).
#[derive(Debug, Clone, PartialEq)]
pub struct BodyValidationReport {
    pub body_item_count: usize,
    pub body_token_estimate: i64,
    pub safe_runway_threshold_tokens: Option<i64>,
    pub protected_pair_count: usize,
    pub reasoning_preserved_count: usize,
}

/// Host-side provenance keys that legitimately differ between the live
/// in-memory history and a record-rebuilt materialization (resume-from-file
/// drops them identically): assigned item ids and Codex-internal chat
/// metadata. Provider payload bytes (arguments, outputs, reasoning content,
/// encrypted fields, call ids, ordering) are NOT in this set.
const HOST_PROVENANCE_KEYS: [&str; 2] = ["id", "internal_chat_message_metadata_passthrough"];

/// Normalized item serialization with host-side provenance keys stripped.
pub fn item_bytes_without_id(item: &ResponseItem) -> String {
    let mut value = serde_json::to_value(item).unwrap_or(Value::Null);
    if let Some(obj) = value.as_object_mut() {
        // serde_json's preserve_order `remove` is a swap-remove and would
        // reorder the remaining keys; rebuild the map to keep field order.
        let filtered: serde_json::Map<String, Value> = obj
            .iter()
            .filter(|(k, _)| !HOST_PROVENANCE_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        *obj = filtered;
    }
    value.to_string()
}

pub fn client_call_id(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.clone()),
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        } => Some(call_id.clone()),
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            execution,
            ..
        } if execution == "client" => Some(call_id.clone()),
        _ => None,
    }
}

pub fn output_call_id(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.clone()),
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some(call_id.clone()),
        _ => None,
    }
}

fn reasoning_encrypted_content(item: &ResponseItem) -> Option<String> {
    if !matches!(item, ResponseItem::Reasoning { .. }) {
        return None;
    }
    serde_json::to_value(item)
        .ok()?
        .get("encrypted_content")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Capture live-history expectations for the protected set and required
/// reasoning before the attempt mutates anything.
///
/// Reasoning expectations cover the current model window: every live
/// [`ResponseItem::Reasoning`] with encrypted content at or after the first
/// protected call's model window (walked back across contiguous model output).
pub fn capture_body_expectations(
    live_items: &[ResponseItem],
    protected_tool_call_ids: &[String],
) -> (Vec<ProtectedPairExpectation>, Vec<String>) {
    let mut pairs = Vec::new();
    for id in protected_tool_call_ids {
        let call = live_items
            .iter()
            .find(|item| client_call_id(item).as_deref() == Some(id.as_str()));
        let output = live_items
            .iter()
            .find(|item| output_call_id(item).as_deref() == Some(id.as_str()));
        if let (Some(call), Some(output)) = (call, output) {
            pairs.push(ProtectedPairExpectation {
                call_id: id.clone(),
                call_bytes: item_bytes_without_id(call),
                output_bytes: item_bytes_without_id(output),
            });
        }
    }

    // Current model window start: index of the first protected call, walked
    // backward over contiguous reasoning / assistant output.
    let first_call_idx = live_items
        .iter()
        .position(|item| {
            client_call_id(item).is_some_and(|id| protected_tool_call_ids.iter().any(|p| p == &id))
        })
        .unwrap_or(live_items.len());
    let mut window_start = first_call_idx;
    while window_start > 0 {
        match &live_items[window_start - 1] {
            ResponseItem::Reasoning { .. } => window_start -= 1,
            ResponseItem::Message { role, .. } if role == "assistant" => window_start -= 1,
            _ => break,
        }
    }
    let required_encrypted_reasoning: Vec<String> = live_items[window_start..]
        .iter()
        .filter_map(reasoning_encrypted_content)
        .collect();

    (pairs, required_encrypted_reasoning)
}

/// Clone a live item with only host provenance stripped (`id` +
/// `internal_chat_message_metadata_passthrough`). Payload bytes — status,
/// ContentItems, name, arguments, outputs — stay intact.
pub fn item_without_host_provenance(item: &ResponseItem) -> Result<ResponseItem, String> {
    serde_json::from_str(&item_bytes_without_id(item))
        .map_err(|e| format!("normalize live protected item: {e}"))
}

/// One protected pair the graft could not prove, left on the materialized
/// LHC-reconstructed items.
#[derive(Debug, Clone, PartialEq)]
pub struct GraftSkip {
    pub call_id: String,
    pub reason: String,
}

/// Per-`call_id` outcome of grafting live provider-native pairs into the
/// materialized body.
///
/// R8 (LIM-103): a graft that cannot be proven is a **degradation, never a
/// stop**. When the exact live pair can't be identified (ambiguous
/// cardinality, missing correlation), the materialized LHC-reconstructed pair
/// stays in the body: same `call_id`, same correlation, missing only
/// provider-specific fields such as `status` and `namespace`. That is a
/// degraded but valid body, and the provider is the final authority on whether
/// it accepts it. A stranded session is not recoverable; a provider rejection
/// is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraftReport {
    /// Protected call ids replaced with the exact live provider-native pair.
    pub grafted: Vec<String>,
    /// Protected call ids left on the LHC-reconstructed pair, with the reason.
    pub degraded: Vec<GraftSkip>,
}

impl GraftReport {
    pub fn is_fully_grafted(&self) -> bool {
        self.degraded.is_empty()
    }

    /// Debuggable one-line summary of every degraded pair (empty when none).
    pub fn degraded_summary(&self) -> String {
        self.degraded
            .iter()
            .map(|skip| format!("{}: {}", skip.call_id, skip.reason))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Replace materialized call/output items with the live in-memory pair for
/// each protected `call_id`, best-effort and per pair.
///
/// Never fails: a pair that cannot be proven byte-for-byte keeps the
/// materialized (LHC-reconstructed) call/output and is reported in
/// [`GraftReport::degraded`] so the caller can warn with specifics.
pub fn graft_live_protected_pairs(
    body: &mut [ResponseItem],
    live_items: &[ResponseItem],
    protected_tool_call_ids: &[String],
) -> GraftReport {
    let mut report = GraftReport::default();
    for id in protected_tool_call_ids {
        let mut skip = |reason: String| {
            report.degraded.push(GraftSkip {
                call_id: id.clone(),
                reason,
            });
        };
        // Require exactly one live call and one live output per protected ID.
        let live_calls: Vec<_> = live_items
            .iter()
            .filter(|item| client_call_id(item).as_deref() == Some(id.as_str()))
            .collect();
        if live_calls.len() != 1 {
            skip(format!(
                "expected exactly 1 live call, found {}",
                live_calls.len()
            ));
            continue;
        }
        let live_outputs: Vec<_> = live_items
            .iter()
            .filter(|item| output_call_id(item).as_deref() == Some(id.as_str()))
            .collect();
        if live_outputs.len() != 1 {
            skip(format!(
                "expected exactly 1 live output, found {}",
                live_outputs.len()
            ));
            continue;
        }
        // Require exactly one materialized call and one materialized output.
        let mat_call_idx: Vec<usize> = body
            .iter()
            .enumerate()
            .filter(|(_, item)| client_call_id(item).as_deref() == Some(id.as_str()))
            .map(|(idx, _)| idx)
            .collect();
        if mat_call_idx.len() != 1 {
            skip(format!(
                "expected exactly 1 call in the materialized body, found {}",
                mat_call_idx.len()
            ));
            continue;
        }
        let mat_out_idx: Vec<usize> = body
            .iter()
            .enumerate()
            .filter(|(_, item)| output_call_id(item).as_deref() == Some(id.as_str()))
            .map(|(idx, _)| idx)
            .collect();
        if mat_out_idx.len() != 1 {
            skip(format!(
                "expected exactly 1 output in the materialized body, found {}",
                mat_out_idx.len()
            ));
            continue;
        }
        // Normalize both sides before mutating so a pair is never half-grafted.
        let (call, output) = match (
            item_without_host_provenance(live_calls[0]),
            item_without_host_provenance(live_outputs[0]),
        ) {
            (Ok(call), Ok(output)) => (call, output),
            (Err(err), _) | (_, Err(err)) => {
                skip(err);
                continue;
            }
        };
        body[mat_call_idx[0]] = call;
        body[mat_out_idx[0]] = output;
        report.grafted.push(id.clone());
    }
    report
}

/// Validate the exact materialized next-request item sequence.
///
/// Checks, in order:
/// 1. provider tool correlation: every client-executed call has exactly one
///    output, every output has a preceding call (order preserved);
/// 2. every protected pair is present exactly once, call before output,
///    byte-stable against the live pre-attempt history;
/// 3. required provider-signed encrypted reasoning survives byte-exact;
/// 4. the complete body's token estimate is strictly below the host
///    safe-runway threshold (when one is configured).
pub fn validate_next_request_body(
    body: &[ResponseItem],
    spec: &BodyValidationSpec,
) -> Result<BodyValidationReport, String> {
    // 1. Correlation and ordering across the whole body.
    let mut call_positions: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut output_positions: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (idx, item) in body.iter().enumerate() {
        if let Some(id) = client_call_id(item) {
            if call_positions.insert(id.clone(), idx).is_some() {
                return Err(format!("duplicate tool call for call_id {id}"));
            }
        }
        if let Some(id) = output_call_id(item) {
            if output_positions.insert(id.clone(), idx).is_some() {
                return Err(format!("duplicate tool output for call_id {id}"));
            }
        }
    }
    for (id, call_idx) in &call_positions {
        match output_positions.get(id) {
            Some(out_idx) if out_idx > call_idx => {}
            Some(_) => return Err(format!("tool output precedes call for call_id {id}")),
            None => return Err(format!("tool call without output for call_id {id}")),
        }
    }
    for id in output_positions.keys() {
        if !call_positions.contains_key(id) {
            return Err(format!("orphan tool output for call_id {id}"));
        }
    }

    // 2. Protected pairs: present, ordered, byte-stable.
    for expected in &spec.protected_pairs {
        let call_idx = call_positions
            .get(&expected.call_id)
            .copied()
            .ok_or_else(|| {
                format!(
                    "protected call {} missing from materialized body",
                    expected.call_id
                )
            })?;
        let out_idx = output_positions
            .get(&expected.call_id)
            .copied()
            .ok_or_else(|| {
                format!(
                    "protected output {} missing from materialized body",
                    expected.call_id
                )
            })?;
        if out_idx <= call_idx {
            return Err(format!(
                "protected pair {} out of order in materialized body",
                expected.call_id
            ));
        }
        let call_bytes = item_bytes_without_id(&body[call_idx]);
        if call_bytes != expected.call_bytes {
            return Err(format!(
                "protected call {} not byte-stable in materialized body",
                expected.call_id
            ));
        }
        let output_bytes = item_bytes_without_id(&body[out_idx]);
        if output_bytes != expected.output_bytes {
            return Err(format!(
                "protected output {} not byte-stable in materialized body",
                expected.call_id
            ));
        }
    }
    if spec.protected_pairs.len() < spec.protected_tool_call_ids.len() {
        // Live capture could not find every pair before the attempt — the
        // materialized body cannot be proven against an incomplete expectation.
        return Err(format!(
            "protected expectations incomplete: {} of {} pairs captured",
            spec.protected_pairs.len(),
            spec.protected_tool_call_ids.len()
        ));
    }

    // 3. Required provider-signed encrypted reasoning survives byte-exact.
    // The certified materializer's canonical text shape (summary/content
    // flattening, identical to resume-from-file) is allowed to differ.
    let body_encrypted: Vec<String> = body
        .iter()
        .filter_map(reasoning_encrypted_content)
        .collect();
    let mut preserved = 0usize;
    for required in &spec.required_encrypted_reasoning {
        if body_encrypted.iter().any(|b| b == required) {
            preserved += 1;
        } else {
            return Err(
                "required encrypted reasoning missing or altered in materialized body".into(),
            );
        }
    }

    // 4. Complete-body size against the host safe-runway threshold.
    let body_token_estimate = crate::estimate_response_items_tokens(body);
    if let Some(threshold) = spec.safe_runway_threshold_tokens
        && body_token_estimate >= threshold
    {
        return Err(format!(
            "materialized body {body_token_estimate} tokens ({BODY_SIZE_SOURCE}) is not below safe-runway threshold {threshold}"
        ));
    }

    Ok(BodyValidationReport {
        body_item_count: body.len(),
        body_token_estimate,
        safe_runway_threshold_tokens: spec.safe_runway_threshold_tokens,
        protected_pair_count: spec.protected_pairs.len(),
        reasoning_preserved_count: preserved,
    })
}

/// What was given up to keep a body sendable.
///
/// R10 (LIM-103): body validation detects; it does not veto. Every failure it
/// can report has a degradation that still produces a provider-legal request,
/// and the provider is the final authority on whether that request is
/// acceptable. A rejected request is recoverable; a stranded session is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyDegradationKind {
    /// A tool call whose output is missing (or ordered after it): both sides
    /// dropped so the request keeps provider-legal correlation.
    DroppedUnpairedToolCall,
    /// A tool output with no matching call in the body.
    DroppedOrphanToolOutput,
    /// A second call/output for a `call_id` already present in the body.
    DroppedDuplicate,
    /// A protected pair the assembled body could not carry at all.
    ProtectedPairUnavailable,
    /// A protected pair present and correlated but not byte-identical to the
    /// live pre-attempt pair — the LHC-reconstructed item (R8).
    ProtectedPairNotByteStable,
    /// Live capture never produced an expectation for a protected call id.
    ProtectedExpectationMissing,
    /// Provider-signed encrypted reasoning that did not survive; omitted.
    MissingEncryptedReasoning,
    /// Oversized content truncated to fit under the host runway threshold.
    TruncatedOversizedContent,
    /// Truncation ran out of candidates before reaching the threshold; the
    /// body is sent oversized and the provider decides (R21).
    StillOverThreshold,
}

impl BodyDegradationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DroppedUnpairedToolCall => "dropped_unpaired_tool_call",
            Self::DroppedOrphanToolOutput => "dropped_orphan_tool_output",
            Self::DroppedDuplicate => "dropped_duplicate",
            Self::ProtectedPairUnavailable => "protected_pair_unavailable",
            Self::ProtectedPairNotByteStable => "protected_pair_not_byte_stable",
            Self::ProtectedExpectationMissing => "protected_expectation_missing",
            Self::MissingEncryptedReasoning => "missing_encrypted_reasoning",
            Self::TruncatedOversizedContent => "truncated_oversized_content",
            Self::StillOverThreshold => "still_over_threshold",
        }
    }
}

/// One degradation with enough specifics to debug it from a log line.
#[derive(Debug, Clone, PartialEq)]
pub struct BodyDegradation {
    pub kind: BodyDegradationKind,
    pub detail: String,
}

impl std::fmt::Display for BodyDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.detail)
    }
}

/// How many degradations a summary line spells out before counting the rest.
const SUMMARY_DETAIL_LIMIT: usize = 24;

/// Render a degradation list as one debuggable log line. Bounded: a body that
/// degrades in hundreds of places still produces a readable warning.
pub fn degradation_summary(degradations: &[BodyDegradation]) -> String {
    let mut line = degradations
        .iter()
        .take(SUMMARY_DETAIL_LIMIT)
        .map(BodyDegradation::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if let Some(rest) = degradations.len().checked_sub(SUMMARY_DETAIL_LIMIT)
        && rest > 0
    {
        line.push_str(&format!("; (+{rest} more)"));
    }
    line
}

/// Marker left in place of content the degrade ladder cut.
pub const TRUNCATION_MARKER: &str = "\n[lhc-compact: content truncated to fit the request]";

/// Text shorter than this is not worth truncating (the marker would dominate).
const MIN_TRUNCATABLE_TEXT: usize = 512;

/// Bound on truncation rounds so a pathological body cannot spin here.
const MAX_TRUNCATION_ROUNDS: usize = 128;

fn floor_char_boundary(text: &str, idx: usize) -> usize {
    if idx >= text.len() {
        return text.len();
    }
    let mut i = idx;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Mutable handles on the model-visible free text of an item.
///
/// Deliberately limited to tool-output bodies and message content: call
/// `arguments` / `input` are structured payloads the provider parses, and
/// truncating them would produce a malformed request rather than a smaller one.
fn item_text_slots(item: &mut ResponseItem) -> Vec<&mut String> {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem;
    match item {
        ResponseItem::Message { content, .. } => content
            .iter_mut()
            .filter_map(|c| match c {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text),
                _ => None,
            })
            .collect(),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => match &mut output.body {
            FunctionCallOutputBody::Text(text) => vec![text],
            FunctionCallOutputBody::ContentItems(items) => items
                .iter_mut()
                .filter_map(|i| match i {
                    FunctionCallOutputContentItem::InputText { text } => Some(text),
                    _ => None,
                })
                .collect(),
        },
        _ => Vec::new(),
    }
}

fn truncatable_text_len(item: &mut ResponseItem) -> usize {
    item_text_slots(item)
        .into_iter()
        .filter(|slot| slot.len() > MIN_TRUNCATABLE_TEXT)
        .map(|slot| slot.len())
        .sum()
}

/// Halve every oversized text slot of one item, leaving [`TRUNCATION_MARKER`]
/// in place of what was cut. Returns the number of characters removed.
fn truncate_item_text_by_half(item: &mut ResponseItem) -> usize {
    let mut removed = 0usize;
    for slot in item_text_slots(item) {
        let len = slot.len();
        if len <= MIN_TRUNCATABLE_TEXT {
            continue;
        }
        let keep = floor_char_boundary(slot, (len / 2).saturating_sub(TRUNCATION_MARKER.len()));
        slot.truncate(keep);
        slot.push_str(TRUNCATION_MARKER);
        removed += len.saturating_sub(slot.len());
    }
    removed
}

fn item_label(item: &ResponseItem) -> String {
    if let Some(id) = client_call_id(item) {
        return format!("tool call {id}");
    }
    if let Some(id) = output_call_id(item) {
        return format!("tool output {id}");
    }
    match item {
        ResponseItem::Message { role, .. } => format!("{role} message"),
        ResponseItem::Reasoning { .. } => "reasoning".to_string(),
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| {
                v.get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "item".to_string()),
    }
}

/// Result of the degrade ladder: the sendable body, which input items survived
/// it, and everything that was given up to get there.
#[derive(Debug, Clone, PartialEq)]
pub struct DegradedBody {
    /// The best body that is still legal to send.
    pub body: Vec<ResponseItem>,
    /// One flag per *input* item: `true` when it survived into `body`.
    /// Callers that mirror the body into durable state (the rollout rewrite)
    /// use this to drop the same items there and keep resume equivalence.
    pub kept: Vec<bool>,
    /// What was given up, with enough specifics to debug from a log line.
    pub degradations: Vec<BodyDegradation>,
}

impl DegradedBody {
    pub fn dropped_count(&self) -> usize {
        self.kept.iter().filter(|kept| !**kept).count()
    }

    pub fn summary(&self) -> String {
        degradation_summary(&self.degradations)
    }
}

/// Reduce an assembled body to the best version that is still legal to send.
///
/// Applies, in order:
/// 1. **correlation repair** — drop duplicate calls/outputs, orphan outputs,
///    calls whose output is missing or ordered before them;
/// 2. **protected pairs** — report pairs that are absent (dropped above) or
///    no longer byte-stable (the LHC-reconstructed pair from a degraded graft,
///    R8); the correlated pair stays in the body;
/// 3. **encrypted reasoning** — report provider-signed reasoning that did not
///    survive materialization; it is simply omitted;
/// 4. **oversized content** — halve the largest model-visible text (protected
///    pairs last) until the body is under the host runway threshold.
///
/// The returned body always satisfies the correlation and ordering rules
/// [`validate_next_request_body`] checks in step 1, so it is structurally
/// sendable. Anything given up to get there is in the degradation list, which
/// the caller must log.
pub fn degrade_body_to_best_available(
    body: &[ResponseItem],
    spec: &BodyValidationSpec,
) -> DegradedBody {
    use std::collections::HashMap;
    use std::collections::HashSet;

    let mut degradations: Vec<BodyDegradation> = Vec::new();
    let protected: HashSet<&str> = spec
        .protected_tool_call_ids
        .iter()
        .map(String::as_str)
        .collect();

    // 1. Correlation repair.
    let mut drop = vec![false; body.len()];
    let mut first_call: HashMap<String, usize> = HashMap::new();
    let mut first_output: HashMap<String, usize> = HashMap::new();
    for (idx, item) in body.iter().enumerate() {
        if let Some(id) = client_call_id(item) {
            if let Some(first) = first_call.get(&id) {
                drop[idx] = true;
                degradations.push(BodyDegradation {
                    kind: BodyDegradationKind::DroppedDuplicate,
                    detail: format!(
                        "duplicate tool call for call_id {id} at position {idx} (kept position {first})"
                    ),
                });
            } else {
                first_call.insert(id, idx);
            }
        }
        if let Some(id) = output_call_id(item) {
            if let Some(first) = first_output.get(&id) {
                drop[idx] = true;
                degradations.push(BodyDegradation {
                    kind: BodyDegradationKind::DroppedDuplicate,
                    detail: format!(
                        "duplicate tool output for call_id {id} at position {idx} (kept position {first})"
                    ),
                });
            } else {
                first_output.insert(id, idx);
            }
        }
    }
    // Deterministic ordering: iterate the body, not the hash maps.
    for (idx, item) in body.iter().enumerate() {
        if drop[idx] {
            continue;
        }
        if let Some(id) = client_call_id(item) {
            match first_output.get(&id) {
                Some(out_idx) if *out_idx > idx => {}
                Some(out_idx) => {
                    drop[idx] = true;
                    drop[*out_idx] = true;
                    degradations.push(BodyDegradation {
                        kind: BodyDegradationKind::DroppedUnpairedToolCall,
                        detail: format!(
                            "tool output for call_id {id} precedes its call (output {out_idx}, call {idx}); dropped both sides"
                        ),
                    });
                }
                None => {
                    drop[idx] = true;
                    degradations.push(BodyDegradation {
                        kind: BodyDegradationKind::DroppedUnpairedToolCall,
                        detail: format!(
                            "tool call {id} at position {idx} has no output in the assembled body; dropped the call"
                        ),
                    });
                }
            }
        }
        if let Some(id) = output_call_id(item)
            && !first_call.contains_key(&id)
        {
            drop[idx] = true;
            degradations.push(BodyDegradation {
                kind: BodyDegradationKind::DroppedOrphanToolOutput,
                detail: format!(
                    "tool output for call_id {id} at position {idx} has no call in the assembled body"
                ),
            });
        }
    }
    let mut degraded: Vec<ResponseItem> = body
        .iter()
        .zip(drop.iter())
        .filter(|(_, dropped)| !**dropped)
        .map(|(item, _)| item.clone())
        .collect();

    // 2. Protected pairs: present-but-degraded is fine; absent is reported.
    for expected in &spec.protected_pairs {
        let call = degraded
            .iter()
            .find(|item| client_call_id(item).as_deref() == Some(expected.call_id.as_str()));
        let output = degraded
            .iter()
            .find(|item| output_call_id(item).as_deref() == Some(expected.call_id.as_str()));
        match (call, output) {
            (Some(call), Some(output)) => {
                if item_bytes_without_id(call) != expected.call_bytes {
                    degradations.push(BodyDegradation {
                        kind: BodyDegradationKind::ProtectedPairNotByteStable,
                        detail: format!(
                            "protected call {} is not the live provider-native item (LHC-reconstructed pair; same call_id and correlation, provider fields such as status/namespace may be absent)",
                            expected.call_id
                        ),
                    });
                }
                if item_bytes_without_id(output) != expected.output_bytes {
                    degradations.push(BodyDegradation {
                        kind: BodyDegradationKind::ProtectedPairNotByteStable,
                        detail: format!(
                            "protected output {} is not the live provider-native item (LHC-reconstructed pair)",
                            expected.call_id
                        ),
                    });
                }
            }
            _ => degradations.push(BodyDegradation {
                kind: BodyDegradationKind::ProtectedPairUnavailable,
                detail: format!(
                    "protected pair {} is not carried by the assembled body (call present: {}, output present: {}); the next request goes without it",
                    expected.call_id,
                    call.is_some(),
                    output.is_some()
                ),
            }),
        }
    }
    let captured: HashSet<&str> = spec
        .protected_pairs
        .iter()
        .map(|pair| pair.call_id.as_str())
        .collect();
    for id in &spec.protected_tool_call_ids {
        if !captured.contains(id.as_str()) {
            degradations.push(BodyDegradation {
                kind: BodyDegradationKind::ProtectedExpectationMissing,
                detail: format!(
                    "live capture produced no byte expectation for protected call {id}; it cannot be proven, only carried"
                ),
            });
        }
    }

    // 3. Encrypted reasoning: omit what did not survive.
    let present: HashSet<String> = degraded
        .iter()
        .filter_map(reasoning_encrypted_content)
        .collect();
    for (idx, required) in spec.required_encrypted_reasoning.iter().enumerate() {
        if !present.contains(required) {
            degradations.push(BodyDegradation {
                kind: BodyDegradationKind::MissingEncryptedReasoning,
                detail: format!(
                    "required encrypted reasoning #{idx} ({} bytes) did not survive materialization; omitted from the request",
                    required.len()
                ),
            });
        }
    }

    // 4. Oversized content: truncate, largest first, protected pairs last.
    if let Some(threshold) = spec.safe_runway_threshold_tokens {
        let mut estimate = crate::estimate_response_items_tokens(&degraded);
        let mut cut_bytes: HashMap<usize, usize> = HashMap::new();
        let mut cut_protected: HashSet<usize> = HashSet::new();
        let mut rounds = 0usize;
        while estimate >= threshold && rounds < MAX_TRUNCATION_ROUNDS {
            rounds += 1;
            // Pick the next victim: unprotected content before protected
            // content, largest text first within each class.
            let mut best: Option<(bool, usize, usize)> = None;
            for (idx, item) in degraded.iter_mut().enumerate() {
                let is_protected = client_call_id(item)
                    .or_else(|| output_call_id(item))
                    .is_some_and(|id| protected.contains(id.as_str()));
                let len = truncatable_text_len(item);
                if len == 0 {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((best_protected, best_len, _)) => {
                        if is_protected == best_protected {
                            len > best_len
                        } else {
                            !is_protected
                        }
                    }
                };
                if better {
                    best = Some((is_protected, len, idx));
                }
            }
            let Some((is_protected, _, idx)) = best else {
                break;
            };
            let removed = truncate_item_text_by_half(&mut degraded[idx]);
            if removed == 0 {
                break;
            }
            *cut_bytes.entry(idx).or_default() += removed;
            if is_protected {
                cut_protected.insert(idx);
            }
            estimate = crate::estimate_response_items_tokens(&degraded);
        }
        let mut cuts: Vec<(usize, usize)> = cut_bytes.into_iter().collect();
        cuts.sort_unstable();
        for (idx, removed) in cuts {
            let protected_note = if cut_protected.contains(&idx) {
                " (protected pair: nothing unprotected left to cut)"
            } else {
                ""
            };
            degradations.push(BodyDegradation {
                kind: BodyDegradationKind::TruncatedOversizedContent,
                detail: format!(
                    "{} at position {idx}{protected_note}: cut {removed} bytes to fit under threshold {threshold} ({BODY_SIZE_SOURCE})",
                    item_label(&degraded[idx])
                ),
            });
        }
        if estimate >= threshold {
            degradations.push(BodyDegradation {
                kind: BodyDegradationKind::StillOverThreshold,
                detail: format!(
                    "body is {estimate} tokens after {rounds} truncation rounds, threshold {threshold} ({BODY_SIZE_SOURCE}); sending anyway - the provider is the final authority"
                ),
            });
        }
    }

    DegradedBody {
        body: degraded,
        kept: drop.iter().map(|dropped| !dropped).collect(),
        degradations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;

    fn call(id: &str, args: &str) -> ResponseItem {
        ResponseItem::FunctionCall {
            id: None,
            name: "tool".into(),
            namespace: None,
            arguments: args.into(),
            encrypted_function_args: None,
            call_id: id.into(),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn output(id: &str, body: &str) -> ResponseItem {
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: id.into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(body.into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn spec_for(live: &[ResponseItem], ids: &[&str], threshold: Option<i64>) -> BodyValidationSpec {
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let (pairs, reasoning) = capture_body_expectations(live, &ids);
        BodyValidationSpec {
            attempt_id: "a1".into(),
            protected_tool_call_ids: ids,
            protected_pairs: pairs,
            required_encrypted_reasoning: reasoning,
            safe_runway_threshold_tokens: threshold,
        }
    }

    #[test]
    fn valid_body_passes_and_reports_size() {
        let live = vec![call("c1", "{\"p\":1}"), output("c1", "r1")];
        let spec = spec_for(&live, &["c1"], Some(100_000));
        let report = validate_next_request_body(&live, &spec).expect("valid");
        assert_eq!(report.protected_pair_count, 1);
        assert!(report.body_token_estimate > 0);
    }

    #[test]
    fn missing_protected_output_fails() {
        let live = vec![call("c1", "{}"), output("c1", "r1")];
        let spec = spec_for(&live, &["c1"], None);
        let body = vec![call("c1", "{}")];
        let err = validate_next_request_body(&body, &spec).unwrap_err();
        assert!(err.contains("without output"), "{err}");
    }

    #[test]
    fn altered_protected_output_fails_byte_stability() {
        let live = vec![call("c1", "{}"), output("c1", "original")];
        let spec = spec_for(&live, &["c1"], None);
        let body = vec![call("c1", "{}"), output("c1", "altered")];
        let err = validate_next_request_body(&body, &spec).unwrap_err();
        assert!(err.contains("not byte-stable"), "{err}");
    }

    #[test]
    fn orphan_output_fails() {
        let live = vec![call("c1", "{}"), output("c1", "r")];
        let spec = spec_for(&live, &["c1"], None);
        let body = vec![call("c1", "{}"), output("c1", "r"), output("ghost", "x")];
        let err = validate_next_request_body(&body, &spec).unwrap_err();
        assert!(err.contains("orphan"), "{err}");
    }

    #[test]
    fn oversized_body_fails_threshold() {
        let live = vec![call("c1", "{}"), output("c1", &"tok ".repeat(500))];
        let spec = spec_for(&live, &["c1"], Some(10));
        let err = validate_next_request_body(&live, &spec).unwrap_err();
        assert!(err.contains("safe-runway"), "{err}");
    }

    #[test]
    fn id_differences_do_not_break_byte_stability() {
        let live = vec![call("c1", "{\"x\":2}"), output("c1", "r")];
        let spec = spec_for(&live, &["c1"], None);
        let mut rebuilt_call = call("c1", "{\"x\":2}");
        rebuilt_call.set_id(Some(codex_protocol::ResponseItemId::new("assigned")));
        let body = vec![rebuilt_call, output("c1", "r")];
        validate_next_request_body(&body, &spec).expect("id-only drift is legal");
    }

    fn custom_call(id: &str, status: Option<&str>, name: &str, input: &str) -> ResponseItem {
        ResponseItem::CustomToolCall {
            id: None,
            status: status.map(str::to_string),
            call_id: id.into(),
            name: name.into(),
            namespace: None,
            input: input.into(),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn custom_output_content_items(id: &str, name: Option<&str>, texts: &[&str]) -> ResponseItem {
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: id.into(),
            name: name.map(str::to_string),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(
                    texts
                        .iter()
                        .map(|text| FunctionCallOutputContentItem::InputText {
                            text: (*text).into(),
                        })
                        .collect(),
                ),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[test]
    fn incident_custom_tool_pair_grafts_and_validates() {
        let live = vec![
            custom_call("c1", Some("completed"), "exec", "{\"cmd\":\"ls\"}"),
            custom_output_content_items("c1", None, &["part-a", "part-b"]),
        ];
        let mut body = vec![
            custom_call("c1", None, "exec", "{\"cmd\":\"ls\"}"),
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "c1".into(),
                name: Some("exec".into()),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text("part-a\npart-b".into()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let report = graft_live_protected_pairs(&mut body, &live, &["c1".into()]);
        assert_eq!(report.grafted, vec!["c1".to_string()]);
        assert!(report.degraded.is_empty(), "{}", report.degraded_summary());
        match &body[0] {
            ResponseItem::CustomToolCall { status, name, .. } => {
                assert_eq!(status.as_deref(), Some("completed"));
                assert_eq!(name, "exec");
            }
            other => panic!("expected CustomToolCall, got {other:?}"),
        }
        match &body[1] {
            ResponseItem::CustomToolCallOutput { name, output, .. } => {
                assert_eq!(name.as_deref(), None);
                assert!(matches!(
                    output.body,
                    FunctionCallOutputBody::ContentItems(ref items) if items.len() == 2
                ));
            }
            other => panic!("expected CustomToolCallOutput, got {other:?}"),
        }
        let spec = spec_for(&live, &["c1"], Some(100_000));
        validate_next_request_body(&body, &spec).expect("grafted incident shape validates");
    }

    /// R8: ambiguous live cardinality degrades to the LHC-reconstructed pair.
    /// The body keeps a correlated call/output for the protected id and stays
    /// sendable; the caller gets a debuggable reason.
    #[test]
    fn duplicate_live_call_cardinality_degrades_to_lhc_pair() {
        let live = vec![call("c1", "{}"), call("c1", "{}"), output("c1", "r")];
        let mut body = vec![call("c1", "{}"), output("c1", "r")];
        let report = graft_live_protected_pairs(&mut body, &live, &["c1".into()]);
        assert!(report.grafted.is_empty());
        assert_eq!(report.degraded.len(), 1);
        assert_eq!(report.degraded[0].call_id, "c1");
        assert!(
            report.degraded[0].reason.contains("expected exactly 1"),
            "{}",
            report.degraded_summary()
        );
        assert_eq!(body.len(), 2, "the materialized pair stays in the body");
        assert_eq!(client_call_id(&body[0]).as_deref(), Some("c1"));
        assert_eq!(output_call_id(&body[1]).as_deref(), Some("c1"));
    }

    #[test]
    fn estimator_counts_large_tool_output_and_rejects_at_threshold() {
        let huge = "x".repeat(10_000);
        let items = vec![output("c1", &huge)];
        let estimate = crate::estimate_response_items_tokens(&items);
        assert!(
            estimate > 1000,
            "o200k estimate of a 10k-char tool result must not look like ~16 tokens: {estimate}"
        );
        let live = vec![call("c1", "{}"), output("c1", &huge)];
        let spec = spec_for(&live, &["c1"], Some(estimate));
        let err = validate_next_request_body(&live, &spec).unwrap_err();
        assert!(err.contains("safe-runway"), "{err}");
        assert!(
            err.contains(&estimate.to_string()),
            "threshold reject should report the same estimate: {err}"
        );
    }

    /// R8: a missing live pair is a degradation, not a stop. Every other
    /// protected id still grafts.
    #[test]
    fn missing_live_pair_degrades_and_other_pairs_still_graft() {
        let live = vec![call("c2", "{\"live\":true}"), output("c2", "live-out")];
        let mut body = vec![
            call("c1", "{}"),
            output("c1", "r"),
            call("c2", "{}"),
            output("c2", "stale"),
        ];
        let report = graft_live_protected_pairs(&mut body, &live, &["c1".into(), "c2".into()]);
        assert_eq!(report.grafted, vec!["c2".to_string()]);
        assert_eq!(report.degraded.len(), 1);
        assert_eq!(report.degraded[0].call_id, "c1");
        assert_eq!(body.len(), 4, "no pair is removed by a failed graft");
        assert_eq!(
            item_bytes_without_id(&body[3]),
            item_bytes_without_id(&output("c2", "live-out")),
            "the provable pair still takes the live bytes"
        );
    }

    // ---------------------------------------------------------------------
    // R10 (LIM-103): validation detects, the degrade ladder keeps compact
    // moving. Every case below proves the degraded body is still legal to
    // send, and that the caller gets specifics to debug from.
    // ---------------------------------------------------------------------

    fn reasoning(encrypted: &str) -> ResponseItem {
        ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: Some(encrypted.into()),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    /// A spec with nothing left to prove: the structural rules every provider
    /// request must satisfy, and nothing else.
    fn structural_spec() -> BodyValidationSpec {
        BodyValidationSpec {
            attempt_id: "structural".into(),
            protected_tool_call_ids: Vec::new(),
            protected_pairs: Vec::new(),
            required_encrypted_reasoning: Vec::new(),
            safe_runway_threshold_tokens: None,
        }
    }

    fn has_kind(degraded: &DegradedBody, kind: BodyDegradationKind) -> bool {
        degraded.degradations.iter().any(|d| d.kind == kind)
    }

    #[test]
    fn degrade_leaves_a_clean_body_untouched() {
        let live = vec![call("c1", "{}"), output("c1", "r")];
        let spec = spec_for(&live, &["c1"], Some(100_000));
        let degraded = degrade_body_to_best_available(&live, &spec);
        assert_eq!(degraded.body, live);
        assert!(degraded.kept.iter().all(|kept| *kept));
        assert!(degraded.degradations.is_empty(), "{}", degraded.summary());
    }

    #[test]
    fn degrade_drops_unpaired_and_orphan_tool_items_into_a_sendable_body() {
        let live = vec![call("c1", "{}"), output("c1", "r")];
        let spec = spec_for(&live, &["c1"], None);
        let body = vec![
            call("lonely", "{}"), // call with no output
            output("ghost", "x"), // output with no call
            call("c1", "{}"),
            output("c1", "r"),
            output("c1", "duplicate"), // duplicate output
        ];
        let degraded = degrade_body_to_best_available(&body, &spec);
        assert_eq!(degraded.kept, vec![false, false, true, true, false]);
        assert_eq!(degraded.body.len(), 2);
        validate_next_request_body(&degraded.body, &spec)
            .expect("degraded body is a legal provider request");
        assert!(has_kind(
            &degraded,
            BodyDegradationKind::DroppedUnpairedToolCall
        ));
        assert!(has_kind(
            &degraded,
            BodyDegradationKind::DroppedOrphanToolOutput
        ));
        assert!(has_kind(&degraded, BodyDegradationKind::DroppedDuplicate));
        let summary = degraded.summary();
        for id in ["lonely", "ghost", "c1"] {
            assert!(summary.contains(id), "summary must name {id}: {summary}");
        }
    }

    #[test]
    fn degrade_drops_an_out_of_order_pair_on_both_sides() {
        let spec = structural_spec();
        let body = vec![output("c1", "r"), call("c1", "{}")];
        let degraded = degrade_body_to_best_available(&body, &spec);
        assert!(degraded.body.is_empty());
        assert_eq!(
            degraded.degradations[0].kind,
            BodyDegradationKind::DroppedUnpairedToolCall
        );
        validate_next_request_body(&degraded.body, &spec).expect("empty body is still legal");
    }

    /// R8 + R10: the LHC-reconstructed pair survives a failed graft, degrade
    /// reports it as not byte-stable, and the pair stays in the request.
    #[test]
    fn degrade_keeps_the_lhc_reconstructed_protected_pair() {
        let live = vec![
            custom_call("c1", Some("completed"), "exec", "{\"cmd\":\"ls\"}"),
            custom_output_content_items("c1", None, &["part-a"]),
        ];
        let spec = spec_for(&live, &["c1"], None);
        // What materialize reconstructs: same call_id, no provider status/name.
        let body = vec![
            custom_call("c1", None, "exec", "{\"cmd\":\"ls\"}"),
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "c1".into(),
                name: Some("exec".into()),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text("part-a".into()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        validate_next_request_body(&body, &spec)
            .expect_err("byte-stability is what fails without the graft");
        let degraded = degrade_body_to_best_available(&body, &spec);
        assert_eq!(degraded.body, body, "the correlated pair is not dropped");
        assert!(has_kind(
            &degraded,
            BodyDegradationKind::ProtectedPairNotByteStable
        ));
        validate_next_request_body(&degraded.body, &structural_spec())
            .expect("degraded protected pair is still a legal provider request");
    }

    #[test]
    fn degrade_reports_a_protected_pair_the_body_cannot_carry() {
        let live = vec![call("c1", "{}"), output("c1", "r")];
        let spec = spec_for(&live, &["c1"], None);
        let body = vec![call("c1", "{}")]; // output never materialized
        let degraded = degrade_body_to_best_available(&body, &spec);
        assert!(degraded.body.is_empty());
        assert!(has_kind(
            &degraded,
            BodyDegradationKind::ProtectedPairUnavailable
        ));
        assert!(degraded.summary().contains("c1"), "{}", degraded.summary());
    }

    #[test]
    fn degrade_reports_an_uncaptured_protected_expectation() {
        let mut spec = structural_spec();
        spec.protected_tool_call_ids = vec!["never-captured".into()];
        let body = vec![call("c1", "{}"), output("c1", "r")];
        let degraded = degrade_body_to_best_available(&body, &spec);
        assert_eq!(degraded.body, body);
        assert_eq!(
            degraded.degradations[0].kind,
            BodyDegradationKind::ProtectedExpectationMissing
        );
    }

    #[test]
    fn degrade_omits_missing_encrypted_reasoning_without_dropping_items() {
        let live = vec![reasoning("signed-abc"), call("c1", "{}"), output("c1", "r")];
        let spec = spec_for(&live, &["c1"], None);
        assert_eq!(spec.required_encrypted_reasoning, vec!["signed-abc"]);
        let body = vec![call("c1", "{}"), output("c1", "r")];
        let degraded = degrade_body_to_best_available(&body, &spec);
        assert_eq!(degraded.body, body, "nothing is dropped for lost reasoning");
        assert_eq!(
            degraded.degradations[0].kind,
            BodyDegradationKind::MissingEncryptedReasoning
        );
        validate_next_request_body(&degraded.body, &structural_spec()).expect("still sendable");
    }

    #[test]
    fn degrade_truncates_oversized_content_under_the_threshold() {
        let huge = "tok ".repeat(20_000);
        let live = vec![call("c1", "{}"), output("c1", &huge)];
        let full = crate::estimate_response_items_tokens(&live);
        let threshold = full / 4;
        let spec = spec_for(&live, &["c1"], Some(threshold));
        validate_next_request_body(&live, &spec).expect_err("oversized body fails validation");
        let degraded = degrade_body_to_best_available(&live, &spec);
        assert!(
            degraded.kept.iter().all(|kept| *kept),
            "truncation drops content, not items"
        );
        let estimate = crate::estimate_response_items_tokens(&degraded.body);
        assert!(
            estimate < threshold,
            "truncated body {estimate} must be under threshold {threshold}"
        );
        assert!(has_kind(
            &degraded,
            BodyDegradationKind::TruncatedOversizedContent
        ));
        assert!(!has_kind(
            &degraded,
            BodyDegradationKind::StillOverThreshold
        ));
        let text = match &degraded.body[1] {
            ResponseItem::FunctionCallOutput { output, .. } => {
                output.body.to_text().expect("text body")
            }
            other => panic!("expected FunctionCallOutput, got {other:?}"),
        };
        assert!(
            text.ends_with(TRUNCATION_MARKER),
            "truncated content must say so: {}",
            &text[text.len().saturating_sub(80)..]
        );
        // Truncating a protected payload is exactly what byte-stability
        // forbids — the point of the ladder is that the request is still
        // structurally legal and the provider decides.
        validate_next_request_body(&degraded.body, &structural_spec())
            .expect("the truncated body is still a legal provider request");
    }

    #[test]
    fn degrade_truncates_unprotected_content_before_protected_content() {
        let filler = "tok ".repeat(20_000);
        let protected_text = "tok ".repeat(20_000);
        let live = vec![
            call("plain", "{}"),
            output("plain", &filler),
            call("c1", "{}"),
            output("c1", &protected_text),
        ];
        let full = crate::estimate_response_items_tokens(&live);
        // Room for one large payload but not two.
        let spec = spec_for(&live, &["c1"], Some(full * 3 / 4));
        let degraded = degrade_body_to_best_available(&live, &spec);
        let unprotected_len = match &degraded.body[1] {
            ResponseItem::FunctionCallOutput { output, .. } => {
                output.body.to_text().expect("text").len()
            }
            other => panic!("unexpected {other:?}"),
        };
        let protected_len = match &degraded.body[3] {
            ResponseItem::FunctionCallOutput { output, .. } => {
                output.body.to_text().expect("text").len()
            }
            other => panic!("unexpected {other:?}"),
        };
        assert!(
            unprotected_len < protected_len,
            "unprotected content is cut first: unprotected {unprotected_len} vs protected {protected_len}"
        );
    }

    /// R21: when there is nothing left to cut, the body is sent oversized with
    /// a warning. The provider is the final authority; a stop is not an option.
    #[test]
    fn degrade_sends_an_oversized_body_when_nothing_is_left_to_cut() {
        let live = vec![call("c1", "{\"a\":1}"), output("c1", "small")];
        let spec = spec_for(&live, &["c1"], Some(1));
        let degraded = degrade_body_to_best_available(&live, &spec);
        assert_eq!(degraded.body, live, "an unshrinkable body still ships");
        assert_eq!(
            degraded.degradations.last().expect("degradation").kind,
            BodyDegradationKind::StillOverThreshold
        );
    }

    /// The degraded body is not just structurally valid on paper: every item
    /// still round-trips through the capture mapping and its own wire format,
    /// so the rewrite, the resume rebuild, and the provider all see the same
    /// items.
    #[test]
    fn degraded_body_round_trips_through_the_mapping() {
        let huge = "tok ".repeat(20_000);
        let live = vec![
            reasoning("signed-abc"),
            call("c1", "{}"),
            output("c1", &huge),
        ];
        let spec_live = spec_for(&live, &["c1"], None);
        let threshold = crate::estimate_response_items_tokens(&live) / 4;
        let spec = BodyValidationSpec {
            safe_runway_threshold_tokens: Some(threshold),
            ..spec_live
        };
        let body = vec![
            call("c1", "{}"),
            output("c1", &huge),
            output("ghost", "orphan"),
        ];
        let degraded = degrade_body_to_best_available(&body, &spec);
        assert!(!degraded.degradations.is_empty());
        validate_next_request_body(&degraded.body, &structural_spec())
            .expect("degraded body is a legal provider request");
        let mut tracker = crate::OccurrenceTracker::new();
        for item in &degraded.body {
            let round_tripped: ResponseItem =
                serde_json::from_str(&serde_json::to_string(item).expect("serialize"))
                    .expect("degraded item round-trips its wire format");
            assert_eq!(
                item_bytes_without_id(&round_tripped),
                item_bytes_without_id(item),
                "degraded item must survive the wire format byte-for-byte"
            );
            let events = crate::map_item(
                "degrade-thread",
                item,
                codex_extension_api::RawItemProvenance::ModelOutput,
                &mut tracker,
                None,
            );
            assert!(
                !events.is_empty(),
                "degraded item must still map into capture events: {item:?}"
            );
        }
    }
}
