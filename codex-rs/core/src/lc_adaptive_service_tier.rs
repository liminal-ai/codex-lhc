use crate::client_common::Prompt;
use crate::context_manager::estimate_item_token_count;
use codex_protocol::config_types::LcAdaptiveServiceTierConfig;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_utils_string::approx_token_count;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LcAdaptiveServiceTierDecision {
    pub(crate) service_tier: Option<String>,
    pub(crate) estimated_context_tokens: Option<i64>,
}

fn normalized_request_tier(value: &str) -> String {
    if value == SERVICE_TIER_DEFAULT_REQUEST_VALUE {
        return value.to_string();
    }
    ServiceTier::from_request_value(value)
        .map(|tier| tier.request_value().to_string())
        .unwrap_or_else(|| value.to_string())
}

fn tier_for_context_tokens(config: &LcAdaptiveServiceTierConfig, context_tokens: i64) -> String {
    let configured = if context_tokens < config.threshold {
        &config.below
    } else {
        &config.at_or_above
    };
    normalized_request_tier(configured)
}

fn estimate_prepared_context_tokens(prompt: &Prompt) -> i64 {
    let instruction_tokens =
        i64::try_from(approx_token_count(&prompt.base_instructions.text)).unwrap_or(i64::MAX);
    let input_tokens = prompt
        .input
        .iter()
        .map(estimate_item_token_count)
        .fold(0_i64, i64::saturating_add);
    let tool_tokens = serde_json::to_string(prompt.tools.as_ref())
        .ok()
        .map(|tools| i64::try_from(approx_token_count(&tools)).unwrap_or(i64::MAX))
        .unwrap_or_default();
    let output_schema_tokens = prompt
        .output_schema
        .as_ref()
        .and_then(|schema| serde_json::to_string(schema).ok())
        .map(|schema| i64::try_from(approx_token_count(&schema)).unwrap_or(i64::MAX))
        .unwrap_or_default();

    instruction_tokens
        .saturating_add(input_tokens)
        .saturating_add(tool_tokens)
        .saturating_add(output_schema_tokens)
}

/// Resolve the request tier without mutating session or operator configuration.
///
/// Context grows between compacts, causing one Fast → Default transition. A
/// later compact produces a smaller prepared request, naturally restoring Fast.
pub(crate) fn resolve_lc_adaptive_service_tier(
    config: &LcAdaptiveServiceTierConfig,
    configured_service_tier: Option<String>,
    prompt: &Prompt,
) -> LcAdaptiveServiceTierDecision {
    if !config.enabled {
        return LcAdaptiveServiceTierDecision {
            service_tier: configured_service_tier,
            estimated_context_tokens: None,
        };
    }

    let estimated_context_tokens = estimate_prepared_context_tokens(prompt);
    LcAdaptiveServiceTierDecision {
        service_tier: Some(tier_for_context_tokens(config, estimated_context_tokens)),
        estimated_context_tokens: Some(estimated_context_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_config() -> LcAdaptiveServiceTierConfig {
        LcAdaptiveServiceTierConfig {
            enabled: true,
            threshold: 272_000,
            below: "fast".to_string(),
            at_or_above: "default".to_string(),
        }
    }

    #[test]
    fn disabled_preserves_manual_service_tier() {
        let decision = resolve_lc_adaptive_service_tier(
            &LcAdaptiveServiceTierConfig::default(),
            Some("priority".to_string()),
            &Prompt::default(),
        );
        assert_eq!(decision.service_tier.as_deref(), Some("priority"));
        assert_eq!(decision.estimated_context_tokens, None);
    }

    #[test]
    fn below_threshold_uses_fast_request_tier() {
        let config = enabled_config();
        assert_eq!(tier_for_context_tokens(&config, 271_999), "priority");
    }

    #[test]
    fn threshold_and_above_use_default_request_tier() {
        let config = enabled_config();
        assert_eq!(tier_for_context_tokens(&config, 272_000), "default");
        assert_eq!(tier_for_context_tokens(&config, 350_000), "default");
    }

    #[test]
    fn compacted_request_naturally_returns_to_fast() {
        let config = enabled_config();
        assert_eq!(tier_for_context_tokens(&config, 300_000), "default");
        assert_eq!(tier_for_context_tokens(&config, 120_000), "priority");
    }
}
