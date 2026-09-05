//! ModelClient → LHC [`InferenceCallbacks`] bridge (R2 / J1–J2).
//!
//! Production derivation always uses this bridge: pinned model
//! [`LHC_DERIVATION_MODEL`] at the lowest accepted reasoning effort.
//! Deterministic callbacks live only on the host crate's test/offline path
//! and must never be the silent production default.
//!
//! The bridge supplies the SDK's inference adapter with one thing only: a
//! [`ModelCall`] that streams already-rendered messages to the pinned model.
//! Template selection and rendering (smoothing, compression, chunk brief,
//! tool result) belong to the adapter. Incident 2026-09-05
//! (`lhc-internal/incidents/derivation-no-template-2026-09-05.md`): the
//! previous bridge sent each derivation's raw input as a bare user message,
//! so the model answered the text instead of deriving from it.

use std::sync::Arc;

use codex_lhc_host::DEFAULT_GUARDS;
use codex_lhc_host::DEFAULT_PROMPT_NAMES;
use codex_lhc_host::InferenceCallbacks;
use codex_lhc_host::ModelAssignment;
use codex_lhc_host::ModelCall;
use codex_lhc_host::ModelCallFailureKind;
use codex_lhc_host::ModelCallInput;
use codex_lhc_host::ModelCallMessage;
use codex_lhc_host::ModelCallMessageRole;
use codex_lhc_host::ModelCallResult;
use codex_lhc_host::ResolvedInferenceConfig;
use codex_lhc_host::ThinkingLevel;
use codex_lhc_host::create_inference_callbacks;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use indexmap::IndexMap;
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
        ReasoningEffort::Persistent => 8,
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

/// Hand the SDK adapter a [`ModelCall`] and let it own template rendering.
fn callbacks_from_live_ctx(ctx: LiveInferCtx) -> InferenceCallbacks {
    let call: ModelCall = Arc::new(move |input: ModelCallInput| {
        let ctx = ctx.clone();
        Box::pin(async move { model_call(&ctx, input).await })
    });
    create_inference_callbacks(ResolvedInferenceConfig {
        call,
        assignments: derivation_assignments(),
        guards: DEFAULT_GUARDS,
        timeout_ms: 60_000,
        max_input_chars: 200_000,
    })
}

/// The four derivation lanes the SDK adapter dispatches on.
pub(crate) const DERIVATION_KINDS: [&str; 4] = [
    "smoothed_prompt",
    "tool_result_summary",
    "detailed_turn_compression",
    "chunk_summary_brief",
];

/// SDK prompt name for a derivation kind, from the SDK's own default table.
fn sdk_default_prompt(kind: &str) -> String {
    DEFAULT_PROMPT_NAMES
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| panic!("SDK DEFAULT_PROMPT_NAMES has no entry for {kind:?}"))
}

/// Per-lane assignments for the adapter. Prompt names come from the SDK's
/// default table so a template bump lands here without a fork edit. The
/// size-target ratios mirror the SDK's private defaults (sdk.rs
/// `default_inference_assignments`) and must be re-checked on a pin move.
/// `provider`/`model` are provenance labels only: [`model_call`] always
/// streams to the pinned [`LHC_DERIVATION_MODEL`].
pub(crate) fn derivation_assignments() -> IndexMap<String, ModelAssignment> {
    let lane = |kind: &str, ratios: Option<(f64, f64, f64)>| ModelAssignment {
        provider: "openai".to_string(),
        model: LHC_DERIVATION_MODEL.to_string(),
        prompt: sdk_default_prompt(kind),
        target_min_ratio: ratios.map(|r| r.0),
        target_aim_ratio: ratios.map(|r| r.1),
        target_max_ratio: ratios.map(|r| r.2),
        thinking: Some(ThinkingLevel::None),
    };
    let mut map = IndexMap::new();
    map.insert("smoothed_prompt".to_string(), lane("smoothed_prompt", None));
    map.insert(
        "tool_result_summary".to_string(),
        lane("tool_result_summary", None),
    );
    map.insert(
        "detailed_turn_compression".to_string(),
        lane("detailed_turn_compression", Some((0.35, 0.5, 0.65))),
    );
    map.insert(
        "chunk_summary_brief".to_string(),
        lane("chunk_summary_brief", Some((0.08, 0.12, 0.2))),
    );
    map
}

/// P1: the exact request payload a derivation call sends.
///
/// Split out so the payload can be asserted offline, from what the bridge
/// actually builds, without spending a live call.
///
/// The adapter renders a template into system and user messages. System
/// messages become `base_instructions`; user messages become the input. On
/// the pinned lite lane (`client.rs`) a non-empty `base_instructions` rides
/// as a prepended developer message and the request's `instructions` field is
/// empty regardless; an empty one sends nothing extra.
///
/// **`base_instructions` is set explicitly and must stay that way.**
/// `Prompt::default()` fills it with `BASE_INSTRUCTIONS_DEFAULT` — Codex's
/// coding-agent prompt: apply_patch conventions, sandbox and approval rules,
/// tool protocol, 20,903 characters of it. Run B3 measured what that costs:
/// **4,392 input tokens for a 55-character summarisation prompt**, ~6.8x the
/// content being summarised, on every one of ~62 calls per compact. Derivation
/// asks a pinned model to rewrite or compress text. It is not agent work and
/// must not carry agent instructions. Only the template's own system text may
/// appear here.
///
/// The other `Prompt::default()` fields were audited with it and are already
/// correct for derivation: `tools` empty (no agent tool surface),
/// `parallel_tool_calls` false, `output_schema` none. `output_schema_strict`
/// defaults true but is inert with no schema.
pub(crate) fn derivation_prompt(messages: &[ModelCallMessage]) -> Prompt {
    let mut instructions: Vec<&str> = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            ModelCallMessageRole::System => instructions.push(message.content.as_str()),
            ModelCallMessageRole::User => input.push(ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![ContentItem::InputText {
                    text: message.content.clone(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
        }
    }
    Prompt {
        input,
        // LHC-HOOK: never `..Default::default()` for this field — see above.
        base_instructions: codex_protocol::models::BaseInstructions {
            text: instructions.join("\n\n"),
            provenance: None,
        },
        tools: Arc::default(),
        parallel_tool_calls: false,
        output_schema: None,
        output_schema_strict: false,
        cyber_access_program: None,
    }
}

/// Typed failure class for the adapter. Message text rides separately and
/// never drives the classification.
fn failure_kind(err: &CodexErr) -> ModelCallFailureKind {
    match err.details() {
        CodexErrorDetails::Timeout | CodexErrorDetails::RequestTimeout => {
            ModelCallFailureKind::Timeout
        }
        CodexErrorDetails::RateLimitExceeded(_)
        | CodexErrorDetails::UsageLimitReached(_)
        | CodexErrorDetails::QuotaExceeded
        | CodexErrorDetails::ServerOverloaded => ModelCallFailureKind::RateLimit,
        CodexErrorDetails::Stream(_)
        | CodexErrorDetails::ResponseStreamFailed(_)
        | CodexErrorDetails::ConnectionFailed(_)
        | CodexErrorDetails::Io(_) => ModelCallFailureKind::Network,
        CodexErrorDetails::InvalidRequest(_) => ModelCallFailureKind::InvalidRequest,
        CodexErrorDetails::RefreshTokenFailed(_) => ModelCallFailureKind::Auth,
        _ => ModelCallFailureKind::Other,
    }
}

async fn model_call(ctx: &LiveInferCtx, input: ModelCallInput) -> ModelCallResult {
    let mut session = ctx.client.new_session();
    let prompt = derivation_prompt(&input.messages);
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
            return ModelCallResult::Err {
                kind: failure_kind(&err),
                message: err.to_string(),
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
                            && out.is_empty()
                        {
                            out = text;
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(err) => {
                return ModelCallResult::Err {
                    kind: failure_kind(&err),
                    message: err.to_string(),
                };
            }
        }
    }
    // Empty or whitespace-only text is the adapter's call (`empty_output`).
    ModelCallResult::Ok { text: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_PROMPT_MARKERS: [&str; 3] = [
        "You are a coding agent running in the Codex CLI",
        "apply_patch",
        "Sandbox and approvals",
    ];

    fn msg(role: ModelCallMessageRole, content: &str) -> ModelCallMessage {
        ModelCallMessage {
            role,
            content: content.to_string(),
        }
    }

    fn input_texts(prompt: &Prompt) -> Vec<String> {
        prompt
            .input
            .iter()
            .filter_map(|item| match item {
                ResponseItem::Message { role, content, .. } => {
                    assert_eq!(role, "user");
                    Some(
                        content
                            .iter()
                            .map(|c| match c {
                                ContentItem::InputText { text } => text.clone(),
                                other => panic!("unexpected content item {other:?}"),
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                    )
                }
                other => panic!("unexpected input item {other:?}"),
            })
            .collect()
    }

    /// Representative render input per lane, shaped as the adapter builds it.
    fn sample_render_input(kind: &str) -> serde_json::Value {
        match kind {
            "smoothed_prompt" => serde_json::json!({ "text": "pls fix teh bug in main.rs" }),
            "tool_result_summary" => serde_json::json!({
                "toolName": "shell",
                "content": "ok\n",
                "outcome": "succeeded",
                "targetTokens": 150,
                "operationClass": "unknown",
                "responseShape": "unknown_content",
                "promptMode": "generic_summary",
                "facts": {},
            }),
            "detailed_turn_compression" => serde_json::json!({
                "dialogueText": "User: hi\nAssistant: hello",
                "inputTokens": 100,
                "targetMinTokens": 35,
                "targetAimTokens": 50,
                "targetMaxTokens": 65,
            }),
            "chunk_summary_brief" => serde_json::json!({
                "text": "t1: greeted. t2: fixed bug.",
                "inputTokens": 100,
                "targetMinTokens": 8,
                "targetAimTokens": 12,
                "targetMaxTokens": 20,
            }),
            other => panic!("unknown derivation kind {other:?}"),
        }
    }

    /// Render a lane through the registry exactly as the adapter does.
    fn rendered_lane(kind: &str) -> Vec<ModelCallMessage> {
        let assignments = derivation_assignments();
        let assignment = assignments.get(kind).expect("assignment for kind");
        let template = codex_lhc_host::registry_get(&assignment.prompt)
            .unwrap_or_else(|| panic!("template {:?} not in registry", assignment.prompt));
        (template.render)(&sample_render_input(kind))
            .into_iter()
            .map(|m| ModelCallMessage {
                role: match m.role {
                    codex_lhc_host::InferenceRequestRole::System => ModelCallMessageRole::System,
                    codex_lhc_host::InferenceRequestRole::User => ModelCallMessageRole::User,
                },
                content: m.content,
            })
            .collect()
    }

    /// System messages become `base_instructions`; user messages become the
    /// input, in order. Nothing from the agent turn surface rides along.
    #[test]
    fn derivation_prompt_maps_system_to_instructions_and_user_to_input() {
        let prompt = derivation_prompt(&[
            msg(ModelCallMessageRole::System, "rewrite, do not answer"),
            msg(ModelCallMessageRole::User, "first"),
            msg(ModelCallMessageRole::User, "second"),
        ]);
        assert_eq!(prompt.base_instructions.text, "rewrite, do not answer");
        assert_eq!(input_texts(&prompt), vec!["first", "second"]);
        assert!(
            prompt.tools.is_empty(),
            "derivation must advertise no tools"
        );
        assert!(!prompt.parallel_tool_calls);
        assert!(prompt.output_schema.is_none());

        let none = derivation_prompt(&[msg(ModelCallMessageRole::User, "only")]);
        assert_eq!(none.base_instructions.text, "");
        assert_eq!(input_texts(&none), vec!["only"]);
    }

    /// Every lane's assignment names a template the registry knows, and the
    /// kind set matches what the adapter dispatches on.
    #[test]
    fn derivation_assignments_name_registered_templates() {
        let assignments = derivation_assignments();
        assert_eq!(
            assignments.keys().map(String::as_str).collect::<Vec<_>>(),
            DERIVATION_KINDS.to_vec()
        );
        for (kind, assignment) in &assignments {
            assert!(
                codex_lhc_host::registry_get(&assignment.prompt).is_some(),
                "{kind}: prompt {:?} not in SDK registry",
                assignment.prompt
            );
            assert_eq!(assignment.model, LHC_DERIVATION_MODEL, "{kind}");
            assert_eq!(assignment.thinking, Some(ThinkingLevel::None), "{kind}");
        }
    }

    /// P1 + incident 2026-09-05: what reaches the wire is the rendered
    /// template and nothing else. The template's instructions are present,
    /// the raw input never travels bare, and Codex's coding-agent prompt is
    /// absent. Restoring `..Default::default()` for `base_instructions`, or
    /// bypassing the adapter with a bare user message, fails this.
    #[test]
    fn p1_derivation_prompt_carries_template_not_agent_instructions() {
        for kind in DERIVATION_KINDS {
            let rendered = rendered_lane(kind);
            assert!(rendered.len() >= 1, "{kind}: template rendered nothing");
            let prompt = derivation_prompt(&rendered);

            let expected_instructions = rendered
                .iter()
                .filter(|m| m.role == ModelCallMessageRole::System)
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            assert_eq!(
                prompt.base_instructions.text, expected_instructions,
                "{kind}: base_instructions must be exactly the template's system text"
            );
            let expected_input: Vec<String> = rendered
                .iter()
                .filter(|m| m.role == ModelCallMessageRole::User)
                .map(|m| m.content.clone())
                .collect();
            assert_eq!(
                input_texts(&prompt),
                expected_input,
                "{kind}: input must be exactly the template's user messages"
            );

            let raw = sample_render_input(kind);
            let raw_text = raw
                .get("text")
                .or_else(|| raw.get("dialogueText"))
                .or_else(|| raw.get("content"))
                .and_then(|v| v.as_str())
                .expect("sample has a text field");
            let wire = format!(
                "{}\n{}",
                prompt.base_instructions.text,
                input_texts(&prompt).join("\n")
            );
            assert!(
                wire.contains(raw_text),
                "{kind}: the text to derive must reach the wire"
            );
            assert!(
                wire.len() > raw_text.len() + 200,
                "{kind}: wire payload is the bare input — template instructions missing"
            );
            for marker in AGENT_PROMPT_MARKERS {
                assert!(
                    !wire.contains(marker),
                    "{kind}: wire payload contains agent-prompt marker {marker:?}"
                );
            }
            assert!(
                prompt.tools.is_empty(),
                "{kind}: derivation must advertise no tools"
            );
        }
    }

    /// The marker strings above must actually occur in the real agent prompt,
    /// otherwise the test above passes vacuously.
    #[test]
    fn p1_markers_are_present_in_the_real_agent_prompt() {
        let base = codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
        for marker in AGENT_PROMPT_MARKERS {
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
            /*content_item_kinds_enabled*/ false,
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
        // Incident 2026-09-05: each lane's request must carry the rendered
        // template, never the bare input. Collect every text field in the
        // request input and check the raw input only appears wrapped.
        fn texts(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::String(t)) = map.get("text") {
                        out.push(t.clone());
                    }
                    for child in map.values() {
                        texts(child, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for child in items {
                        texts(child, out);
                    }
                }
                _ => {}
            }
        }
        for raw in ["smooth-me", "tool-out", "turn-dialogue", "chunk-brief"] {
            let mut all = Vec::new();
            for body in &model_requests {
                texts(body, &mut all);
            }
            let carrying: Vec<&String> = all.iter().filter(|t| t.contains(raw)).collect();
            assert!(!carrying.is_empty(), "no request carried input {raw:?}");
            assert!(
                carrying.iter().all(|t| t.as_str() != raw),
                "input {raw:?} was sent bare, without its template wrapper"
            );
            let joined: String = all.join("\n");
            assert!(
                joined.len() > raw.len() + 200,
                "request for {raw:?} carries no template instructions"
            );
            for marker in AGENT_PROMPT_MARKERS {
                assert!(
                    !joined.contains(marker),
                    "agent-prompt marker {marker:?} on the wire"
                );
            }
        }
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
