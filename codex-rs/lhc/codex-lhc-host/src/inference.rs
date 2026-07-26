//! Inference wiring for LHC derivation / compact.
//!
//! - **Deterministic** (`live = false`): offline tests and capture open.
//! - **Live** (`live = true`): **fails closed** unless the host injects real
//!   callbacks via [`produce_lhc_compact`] with a ModelClient-built
//!   [`InferenceCallbacks`]. This function never silently returns canned text
//!   for the live arm (R2).
//!
//! The ModelClient → InferenceCallbacks adapter lives in `codex-core`
//! (`lhc_model_inference_callbacks`) so the adapter crate does not take a
//! reverse dependency on core.

use lhc::shared_tech::InferenceCallbacks;
use lhc::shared_tech::create_deterministic_inference_callbacks;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LhcInferenceError {
    /// Live arm requested but no host ModelClient bridge was provided.
    LiveNotConfigured,
}

impl std::fmt::Display for LhcInferenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LiveNotConfigured => write!(
                f,
                "live inference requested but not configured: host must pass \
                 ModelClient-backed InferenceCallbacks into produce_lhc_compact \
                 (lhc_inference_callbacks(true) never returns deterministic text)"
            ),
        }
    }
}

impl std::error::Error for LhcInferenceError {}

/// Build inference callbacks.
///
/// * `live = false` → deterministic (offline / tests).
/// * `live = true` → **error** (fail closed). Use core's
///   `lhc_model_inference_callbacks` for the real bridge.
pub fn lhc_inference_callbacks(live: bool) -> Result<InferenceCallbacks, LhcInferenceError> {
    if live {
        return Err(LhcInferenceError::LiveNotConfigured);
    }
    Ok(create_deterministic_inference_callbacks())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_ok() {
        assert!(lhc_inference_callbacks(false).is_ok());
    }

    #[test]
    fn live_fails_closed_not_deterministic() {
        match lhc_inference_callbacks(true) {
            Ok(_) => panic!("live must not return deterministic callbacks"),
            Err(err) => assert_eq!(err, LhcInferenceError::LiveNotConfigured),
        }
    }
}
