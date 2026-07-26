//! ModelClient → LHC [`InferenceCallbacks`] bridge (R2 / J1–J2).
//!
//! Production derivation always uses this bridge: pinned model
//! [`LHC_DERIVATION_MODEL`] at the lowest accepted reasoning effort.
//! Deterministic callbacks live only on the host crate's test/offline path
//! and must never be the silent production default.

use std::sync::Arc;

use codex_lhc_host::CompressDetailedTurnInput;
use codex_lhc_host::InferenceCallbacks;
use codex_lhc_host::InferenceResult;
use codex_lhc_host::SmoothPromptInput;
use codex_lhc_host::SummarizeChunkBriefInput;
use codex_lhc_host::SummarizeToolResultInput;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use tracing::info;
use tracing::warn;

use crate::client::ModelClient;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::session::session::Session;

/// Pinned derivation model for both small-op and big-op lanes (ruling 8c3ca18).
pub(crate) const LHC_DERIVATION_MODEL: &str = "gpt-5.6-luna";

/// Resolved derivation target: model metadata + pinned lowest effort.
#[derive(Debug, Clone)]
pub(crate) struct LhcDerivationTarget {
    pub model_info: ModelInfo,
    pub effort: ReasoningEffort,
}

/// Rank for selecting the minimum supported effort (lower = cheaper/faster).
fn effort_rank(effort: &ReasoningEffort) -> u8 {
    match effort {
        ReasoningEffort::None => 0,
        ReasoningEffort::Minimal => 1,
        ReasoningEffort::Low => 2,
        ReasoningEffort::Medium => 3,
        ReasoningEffort::High => 4,
        ReasoningEffort::XHigh => 5,
        ReasoningEffort::Max => 6,
        ReasoningEffort::Ultra => 7,
        ReasoningEffort::Custom(_) => 100,
    }
}

/// Prefer [`ReasoningEffort::None`]; else the minimum listed for the model.
/// Logs when falling back so silent higher-effort landings are visible.
pub(crate) fn resolve_lhc_derivation_effort(model_info: &ModelInfo) -> ReasoningEffort {
    let supported: Vec<ReasoningEffort> = model_info
        .supported_reasoning_levels
        .iter()
        .map(|p| p.effort.clone())
        .collect();

    if supported.is_empty() {
        info!(
            model = %model_info.slug,
            "LHC derivation: no supported_reasoning_levels; using ReasoningEffort::None"
        );
        return ReasoningEffort::None;
    }

    if supported.iter().any(|e| matches!(e, ReasoningEffort::None)) {
        return ReasoningEffort::None;
    }

    let min = supported
        .into_iter()
        .min_by_key(effort_rank)
        .unwrap_or(ReasoningEffort::Low);
    warn!(
        model = %model_info.slug,
        effort = %min,
        "LHC derivation: ReasoningEffort::None not accepted by model; using minimum supported effort"
    );
    min
}

/// Resolve pinned derivation model + effort for the session's provider.
/// Fails open (Err) when the model is not in the catalog — never falls back
/// to the user's turn model.
pub(crate) async fn resolve_lhc_derivation_target(
    sess: &Session,
) -> Result<LhcDerivationTarget, String> {
    let config = sess.get_config().await;
    let model_info = sess
        .services
        .models_manager
        .get_model_info(LHC_DERIVATION_MODEL, &config.to_models_manager_config())
        .await;
    if model_info.used_fallback_model_metadata {
        return Err(format!(
            "derivation model `{LHC_DERIVATION_MODEL}` unavailable for provider \
             (fallback metadata only); failing open — not using turn model"
        ));
    }
    if model_info.slug != LHC_DERIVATION_MODEL
        && !model_info.slug.ends_with(LHC_DERIVATION_MODEL)
        && !model_info.slug.contains(LHC_DERIVATION_MODEL)
    {
        // Prefix match from models manager may return a longer slug; require
        // the pinned name is still the identity.
        if !model_info.slug.contains("luna") {
            return Err(format!(
                "derivation model resolve returned unexpected slug `{}` \
                 (wanted `{LHC_DERIVATION_MODEL}`)",
                model_info.slug
            ));
        }
    }
    let effort = resolve_lhc_derivation_effort(&model_info);
    Ok(LhcDerivationTarget { model_info, effort })
}

#[derive(Clone)]
struct LiveInferCtx {
    client: ModelClient,
    model_info: ModelInfo,
    effort: ReasoningEffort,
    telemetry: SessionTelemetry,
    installation_id: String,
    session_id: String,
    thread_id: String,
    window_id: String,
}

impl LiveInferCtx {
    async fn complete(&self, text: &str) -> InferenceResult {
        model_complete_text(self, text).await
    }
}

/// Build live inference callbacks for production (pinned model + lowest effort).
/// Returns Err when the derivation model is unavailable — callers fail open.
pub(crate) async fn try_lhc_model_inference_callbacks(
    sess: &Session,
) -> Result<InferenceCallbacks, String> {
    let target = resolve_lhc_derivation_target(sess).await?;
    Ok(lhc_model_inference_callbacks_from_target(sess, target))
}

/// Build callbacks from an already-resolved target (testable seam).
pub(crate) fn lhc_model_inference_callbacks_from_target(
    sess: &Session,
    target: LhcDerivationTarget,
) -> InferenceCallbacks {
    let ctx = LiveInferCtx {
        client: sess.services.model_client.clone(),
        model_info: target.model_info,
        effort: target.effort,
        telemetry: sess.services.session_telemetry.clone(),
        installation_id: sess.installation_id.clone(),
        session_id: sess.session_id().to_string(),
        thread_id: sess.thread_id().to_string(),
        window_id: format!("{}:lhc-infer", sess.thread_id()),
    };
    callbacks_from_live_ctx(ctx)
}

fn callbacks_from_live_ctx(ctx: LiveInferCtx) -> InferenceCallbacks {
    let a = ctx.clone();
    let b = ctx.clone();
    let c = ctx.clone();
    let d = ctx;
    InferenceCallbacks {
        smooth_prompt: Arc::new(move |input: SmoothPromptInput| {
            let ctx = a.clone();
            Box::pin(async move { ctx.complete(&input.text).await })
        }),
        summarize_tool_result: Arc::new(move |input: SummarizeToolResultInput| {
            let ctx = b.clone();
            Box::pin(async move { ctx.complete(&input.content).await })
        }),
        compress_detailed_turn: Arc::new(move |input: CompressDetailedTurnInput| {
            let ctx = c.clone();
            Box::pin(async move { ctx.complete(&input.dialogue_text).await })
        }),
        summarize_chunk_brief: Arc::new(move |input: SummarizeChunkBriefInput| {
            let ctx = d.clone();
            Box::pin(async move { ctx.complete(&input.text).await })
        }),
    }
}

/// P1: the exact request payload a derivation call sends.
///
/// Split out so the payload can be asserted offline, from what the bridge
/// actually builds, without spending a live call.
///
/// **`base_instructions` is set explicitly and must stay that way.**
/// `Prompt::default()` fills it with `BASE_INSTRUCTIONS_DEFAULT` — Codex's
/// coding-agent prompt: apply_patch conventions, sandbox and approval rules,
/// tool protocol, 20,903 characters of it. Run B3 measured what that costs:
/// **4,392 input tokens for a 55-character summarisation prompt**, ~6.8x the
/// content being summarised, on every one of ~62 calls per compact. Derivation
/// asks a pinned model to compress a conversation turn. It is not agent work
/// and must not carry agent instructions.
///
/// Empty is the right value here rather than a short instruction string,
/// because the pinned lane makes it provably a no-op: `gpt-5.6-luna` is a
/// `use_responses_lite` model, and on that path (`client.rs`) the request's
/// `instructions` field is `String::new()` regardless, while
/// `base_instructions` rides as a prepended developer message *only when
/// non-empty*. Empty therefore removes the message and sends nothing extra.
///
/// If the derivation model ever moves to a non-lite model, `instructions: ""`
/// would reach the wire directly — re-check that the provider accepts it before
/// making that change.
///
/// The other `Prompt::default()` fields were audited with it and are already
/// correct for derivation: `tools` empty (no agent tool surface),
/// `parallel_tool_calls` false, `output_schema` none. `output_schema_strict`
/// defaults true but is inert with no schema.
pub(crate) fn derivation_prompt(text: &str) -> Prompt {
    Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        // LHC-HOOK: never `..Default::default()` for this field — see above.
        base_instructions: codex_protocol::models::BaseInstructions {
            text: String::new(),
        },
        tools: Vec::new(),
        parallel_tool_calls: false,
        output_schema: None,
        output_schema_strict: false,
    }
}

async fn model_complete_text(ctx: &LiveInferCtx, text: &str) -> InferenceResult {
    let mut session = ctx.client.new_session();
    let prompt = derivation_prompt(text);
    let meta = CodexResponsesMetadata::new(
        ctx.installation_id.clone(),
        ctx.session_id.clone(),
        ctx.thread_id.clone(),
        ctx.window_id.clone(),
    );
    // Explicit Some(effort): Option::None would mean "unspecified" and fall
    // back to the model's default (often medium) — not lowest effort.
    let stream = match session
        .stream(
            &prompt,
            &ctx.model_info,
            &ctx.telemetry,
            Some(ctx.effort.clone()),
            ReasoningSummaryConfig::None,
            /*service_tier*/ None,
            &meta,
            &InferenceTraceContext::disabled(),
        )
        .await
    {
        Ok(s) => s,
        Err(err) => {
            warn!(%err, model = %ctx.model_info.slug, effort = %ctx.effort, "LHC live ModelClient stream failed");
            return InferenceResult::Err {
                reason: err.to_string(),
                request_messages: None,
            };
        }
    };

    let mut out = String::new();
    let mut stream = stream;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ResponseEvent::OutputTextDelta(delta)) => out.push_str(&delta),
            Ok(ResponseEvent::OutputItemDone(item)) => {
                if let ResponseItem::Message { content, .. } = item {
                    for part in content {
                        if let ContentItem::OutputText { text } = part
                            && out.is_empty() {
                                out = text;
                            }
                    }
                }
            }
            Ok(_) => {}
            Err(err) => {
                return InferenceResult::Err {
                    reason: err.to_string(),
                    request_messages: None,
                };
            }
        }
    }
    if out.is_empty() {
        InferenceResult::Err {
            reason: "empty model response for LHC inference".into(),
            request_messages: None,
        }
    } else {
        InferenceResult::Ok {
            text: out,
            provenance: None,
            request_messages: None,
            raw_response: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1: a derivation request must not carry Codex's coding-agent prompt.
    ///
    /// Counted from the payload the bridge actually builds
    /// ([`derivation_prompt`]), not a hand-made fixture. Restoring
    /// `..Default::default()` for `base_instructions` fails this.
    ///
    /// Run B3 measured the cost of getting this wrong: 4,392 input tokens for
    /// a 55-character prompt, ~62 calls per compact.
    #[test]
    fn p1_derivation_prompt_carries_no_agent_instructions() {
        let content = "summarise this turn";
        let prompt = derivation_prompt(content);
        let instructions = &prompt.base_instructions.text;

        assert!(
            instructions.len() < 1_000,
            "derivation instructions must stay tiny, got {} chars — \
             `Prompt::default()` puts BASE_INSTRUCTIONS_DEFAULT ({} chars) here",
            instructions.len(),
            codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT.len()
        );

        // Distinctive opening line of BASE_INSTRUCTIONS_DEFAULT. If the agent
        // prompt is reinstated by any route, this catches it.
        for marker in [
            "You are a coding agent running in the Codex CLI",
            "apply_patch",
            "Sandbox and approvals",
        ] {
            assert!(
                !instructions.contains(marker),
                "derivation instructions contain agent-prompt marker {marker:?}"
            );
        }

        // Nothing else from the agent turn surface leaks in either.
        assert!(
            prompt.tools.is_empty(),
            "derivation must advertise no tools"
        );
        assert!(!prompt.parallel_tool_calls);
        assert!(prompt.output_schema.is_none());

        // The request is the content, plus a negligible envelope.
        let input_chars: usize = prompt
            .input
            .iter()
            .map(|item| match item {
                ResponseItem::Message { content, .. } => content
                    .iter()
                    .map(|c| match c {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            text.len()
                        }
                        _ => 0,
                    })
                    .sum(),
                _ => 0,
            })
            .sum();
        assert_eq!(
            input_chars,
            content.len(),
            "derivation input must be exactly the text to derive"
        );
        assert!(
            instructions.len() + input_chars < content.len() + 1_000,
            "total derivation payload must be dominated by the content itself"
        );
    }

    /// The marker strings above must actually occur in the real agent prompt,
    /// otherwise the test above passes vacuously.
    #[test]
    fn p1_markers_are_present_in_the_real_agent_prompt() {
        let base = codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
        for marker in [
            "You are a coding agent running in the Codex CLI",
            "apply_patch",
            "Sandbox and approvals",
        ] {
            assert!(
                base.contains(marker),
                "marker {marker:?} no longer appears in BASE_INSTRUCTIONS_DEFAULT — \
                 p1_derivation_prompt_carries_no_agent_instructions is now vacuous"
            );
        }
    }

    use codex_lhc_host::lhc_inference_callbacks;
    use codex_models_manager::ModelsManagerConfig;
    use codex_models_manager::test_support::construct_model_info_offline_for_tests;
    use codex_protocol::openai_models::ReasoningEffortPreset;
    use pretty_assertions::assert_eq;

    fn model_with_levels(levels: Vec<ReasoningEffort>) -> ModelInfo {
        let mut model = construct_model_info_offline_for_tests(
            LHC_DERIVATION_MODEL,
            &ModelsManagerConfig::default(),
        );
        model.supported_reasoning_levels = levels
            .into_iter()
            .map(|effort| ReasoningEffortPreset {
                effort,
                description: "t".into(),
            })
            .collect();
        model
    }

    #[test]
    fn host_live_flag_fails_closed() {
        assert!(lhc_inference_callbacks(true).is_err());
    }

    #[test]
    fn derivation_model_slug_is_pinned_luna() {
        assert_eq!(LHC_DERIVATION_MODEL, "gpt-5.6-luna");
    }

    #[test]
    fn effort_prefers_none_when_supported() {
        let model = model_with_levels(vec![
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
        ]);
        assert_eq!(resolve_lhc_derivation_effort(&model), ReasoningEffort::None);
    }

    #[test]
    fn effort_falls_back_to_minimum_when_none_unsupported() {
        // Luna catalog shape: low..max, no none.
        let model = model_with_levels(vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]);
        assert_eq!(resolve_lhc_derivation_effort(&model), ReasoningEffort::Low);
    }

    #[test]
    fn effort_empty_supported_uses_none() {
        let model = model_with_levels(vec![]);
        assert_eq!(resolve_lhc_derivation_effort(&model), ReasoningEffort::None);
    }

    /// J2: every one of the four callbacks streams with pinned model + lowest effort.
    #[tokio::test]
    async fn j2_all_four_callbacks_request_pinned_model_and_lowest_effort() {
        use codex_http_client::HttpClientFactory;
        use codex_http_client::OutboundProxyPolicy;
        use codex_login::auth::AgentIdentityAuthPolicy;
        use codex_model_provider_info::ModelProviderInfo;
        use codex_protocol::ThreadId;
        use codex_protocol::protocol::SessionSource;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path_regex;

        use crate::client::ModelClient;
        use crate::session::tests::make_session_and_context;
        use codex_lhc_host::CompressDetailedTurnInput;
        use codex_lhc_host::SmoothPromptInput;
        use codex_lhc_host::SummarizeChunkBriefInput;
        use codex_lhc_host::SummarizeToolResultInput;

        let server = MockServer::start().await;
        // Minimal SSE: created + assistant message + completed.
        let sse_body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path_regex(".*/responses.*"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&server)
            .await;

        let (mut session, _tc) = make_session_and_context().await;
        let mut provider =
            ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
        // Force HTTP so wiremock can capture request bodies (no WS upgrade).
        provider.supports_websockets = false;
        session.services.model_client = ModelClient::new(
            /*auth_manager*/ None,
            AgentIdentityAuthPolicy::JwtOnly,
            ThreadId::new(),
            provider,
            SessionSource::Exec,
            "test_originator".to_string(),
            /*model_verbosity*/ None,
            /*enable_request_compression*/ false,
            /*include_timing_metrics*/ false,
            /*beta_features_header*/ None,
            /*concurrent_reasoning_summaries_enabled*/ false,
            /*attestation_provider*/ None,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        );

        let target = resolve_lhc_derivation_target(&session)
            .await
            .expect("luna from catalog");
        let expected_effort = target.effort.clone();
        assert_eq!(expected_effort, ReasoningEffort::Low);
        let callbacks = lhc_model_inference_callbacks_from_target(&session, target);

        // Invoke all four lanes.
        let r1 = (callbacks.smooth_prompt)(SmoothPromptInput {
            text: "smooth-me".into(),
        })
        .await;
        let r2 = (callbacks.summarize_tool_result)(SummarizeToolResultInput {
            tool_name: "shell".into(),
            content: "tool-out".into(),
            outcome: None,
            target_tokens: None,
            operation_class: None,
            response_shape: None,
            prompt_mode: None,
            facts: None,
        })
        .await;
        let r3 = (callbacks.compress_detailed_turn)(CompressDetailedTurnInput {
            dialogue_text: "turn-dialogue".into(),
            input_tokens: 100,
            target_min_tokens: 10,
            target_aim_tokens: 20,
            target_max_tokens: 40,
        })
        .await;
        let r4 = (callbacks.summarize_chunk_brief)(SummarizeChunkBriefInput {
            text: "chunk-brief".into(),
            input_tokens: 100,
            target_min_tokens: 10,
            target_aim_tokens: 20,
            target_max_tokens: 40,
        })
        .await;
        // At least attempt streams; some may Err if SSE shape mismatches — still check requests.
        let _ = (r1, r2, r3, r4);

        let requests = server.received_requests().await.expect("requests");
        let model_requests: Vec<_> = requests
            .iter()
            .filter_map(|req| {
                if req.body.is_empty() {
                    return None;
                }
                let body: serde_json::Value = serde_json::from_slice(&req.body).ok()?;
                body.get("model")?;
                Some(body)
            })
            .collect();
        assert!(
            model_requests.len() >= 4,
            "expected ≥4 derivation POSTs with model field, got {} (total reqs={}); sample paths={:?}",
            model_requests.len(),
            requests.len(),
            requests
                .iter()
                .map(|r| r.url.path().to_string())
                .collect::<Vec<_>>()
        );
        for (i, body) in model_requests.iter().enumerate() {
            let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
            assert!(
                model.contains("luna") || model == LHC_DERIVATION_MODEL,
                "request {i}: model must be pinned luna, got {model:?} body={body}"
            );
            let effort = body
                .pointer("/reasoning/effort")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            assert_eq!(
                effort,
                expected_effort.as_str(),
                "request {i}: effort must be {}, got {effort:?} body={body}",
                expected_effort.as_str()
            );
        }
    }
}
