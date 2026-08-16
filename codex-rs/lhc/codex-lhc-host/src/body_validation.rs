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

/// Replace materialized call/output items with the live in-memory pair for
/// each protected `call_id`. Missing live or materialized sides are a hard
/// error — the pair must have been preserved after the visibility boundary.
pub fn graft_live_protected_pairs(
    body: &mut [ResponseItem],
    live_items: &[ResponseItem],
    protected_tool_call_ids: &[String],
) -> Result<usize, String> {
    let mut grafted = 0usize;
    for id in protected_tool_call_ids {
        // Require exactly one live call and one live output per protected ID.
        let live_calls: Vec<_> = live_items
            .iter()
            .filter(|item| client_call_id(item).as_deref() == Some(id.as_str()))
            .collect();
        if live_calls.len() != 1 {
            return Err(format!(
                "protected call {id}: expected exactly 1 in live history, found {}",
                live_calls.len()
            ));
        }
        let live_call = live_calls[0];
        let live_outputs: Vec<_> = live_items
            .iter()
            .filter(|item| output_call_id(item).as_deref() == Some(id.as_str()))
            .collect();
        if live_outputs.len() != 1 {
            return Err(format!(
                "protected output {id}: expected exactly 1 in live history, found {}",
                live_outputs.len()
            ));
        }
        let live_output = live_outputs[0];
        // Require exactly one materialized call and one materialized output.
        let mat_calls: Vec<_> = body
            .iter()
            .enumerate()
            .filter(|(_, item)| client_call_id(item).as_deref() == Some(id.as_str()))
            .collect();
        if mat_calls.len() != 1 {
            return Err(format!(
                "protected call {id}: expected exactly 1 in materialized body, found {}",
                mat_calls.len()
            ));
        }
        let call_idx = mat_calls[0].0;
        let mat_outputs: Vec<_> = body
            .iter()
            .enumerate()
            .filter(|(_, item)| output_call_id(item).as_deref() == Some(id.as_str()))
            .collect();
        if mat_outputs.len() != 1 {
            return Err(format!(
                "protected output {id}: expected exactly 1 in materialized body, found {}",
                mat_outputs.len()
            ));
        }
        let out_idx = mat_outputs[0].0;
        body[call_idx] = item_without_host_provenance(live_call)?;
        body[out_idx] = item_without_host_provenance(live_output)?;
        grafted += 1;
    }
    Ok(grafted)
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
        let grafted = graft_live_protected_pairs(&mut body, &live, &["c1".into()]).expect("graft");
        assert_eq!(grafted, 1);
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

    #[test]
    fn duplicate_live_call_cardinality_fails_graft() {
        let live = vec![call("c1", "{}"), call("c1", "{}"), output("c1", "r")];
        let mut body = vec![call("c1", "{}"), output("c1", "r")];
        let err = graft_live_protected_pairs(&mut body, &live, &["c1".into()]).unwrap_err();
        assert!(err.contains("expected exactly 1"), "{err}");
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

    #[test]
    fn missing_live_pair_fails_graft() {
        let mut body = vec![call("c1", "{}"), output("c1", "r")];
        let err = graft_live_protected_pairs(&mut body, &[], &["c1".into()]).unwrap_err();
        assert!(err.contains("expected exactly 1"), "{err}");
    }
}
