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

/// Host-seeded production callbacks, late-bound.
///
/// Background mode means LHC's own scheduler drains derivation as intake
/// commits — so the **capture** session's callbacks are what lands in the
/// durable record. They must never be the deterministic ones: whatever they
/// produce is later served as if it were real derivation (J1).
///
/// The host cannot supply real callbacks at `on_thread_start` (resolving the
/// derivation model needs the models manager and can fail), so the capture
/// session is opened with these, and the host seeds the real ones through the
/// existing lifecycle seam.
///
/// **Waiting, not erroring, is the load-bearing choice.** LHC's `work_item`
/// table has no `attempts` column — it was dropped in `thread_migrate.rs` — so
/// a returned `InferenceResult::Err` is a *terminal* failure for that
/// derivation, not a retry. Erroring before the host has seeded would
/// permanently un-derive every turn of a thread's opening minutes. Awaiting
/// holds the work item's claim instead, which is a state LHC already models
/// (`DrainStoppedBecause::InFlight` plus claim-expiry wake timers).
///
/// The wait is **unbounded**, and that is deliberate. A terminal failure is
/// permanent for that derivation, and the compact arm now treats any terminal
/// failure as a refusal (L2) — so timing out here would poison a thread for its
/// whole life over a transient startup race. Instead the derivation simply
/// waits; if the model never becomes available, the *caller's* bounded
/// `drain_settled` fails open and LHC's claim lease releases the work. Bounded
/// where a user is waiting, patient where nobody is.
#[derive(Clone)]
pub struct LateBoundCallbacks {
    slot: std::sync::Arc<std::sync::Mutex<Option<InferenceCallbacks>>>,
    ready: std::sync::Arc<tokio::sync::Notify>,
}

impl LateBoundCallbacks {
    pub fn new() -> Self {
        Self {
            slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
            ready: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Install the host's production callbacks and wake anything waiting.
    pub fn seed(&self, callbacks: InferenceCallbacks) {
        *self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(callbacks);
        self.ready.notify_waiters();
    }

    /// Already-seeded binding — for callers that have callbacks up front.
    pub fn seeded(callbacks: InferenceCallbacks) -> Self {
        let this = Self::new();
        this.seed(callbacks);
        this
    }

    pub fn is_seeded(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn get(&self) -> Option<InferenceCallbacks> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn resolve(&self) -> InferenceCallbacks {
        loop {
            if let Some(cb) = self.get() {
                return cb;
            }
            let notified = self.ready.notified();
            // Re-check after arming so a seed racing the arm is not missed.
            if let Some(cb) = self.get() {
                return cb;
            }
            notified.await;
        }
    }

    /// Build `InferenceCallbacks` that route through this late binding.
    pub fn callbacks(&self) -> InferenceCallbacks {
        use lhc::shared_tech::CompressDetailedTurnInput;
        use lhc::shared_tech::SmoothPromptInput;
        use lhc::shared_tech::SummarizeChunkBriefInput;
        use lhc::shared_tech::SummarizeToolResultInput;

        macro_rules! route {
            ($field:ident, $ty:ty) => {{
                let binding = self.clone();
                std::sync::Arc::new(move |input: $ty| {
                    let binding = binding.clone();
                    Box::pin(async move {
                        let cb = binding.resolve().await;
                        (cb.$field)(input).await
                    })
                        as lhc::shared_tech::derivation::BoxFuture<
                            lhc::shared_tech::InferenceResult,
                        >
                })
            }};
        }
        InferenceCallbacks {
            smooth_prompt: route!(smooth_prompt, SmoothPromptInput),
            summarize_tool_result: route!(summarize_tool_result, SummarizeToolResultInput),
            compress_detailed_turn: route!(compress_detailed_turn, CompressDetailedTurnInput),
            summarize_chunk_brief: route!(summarize_chunk_brief, SummarizeChunkBriefInput),
        }
    }
}

impl Default for LateBoundCallbacks {
    fn default() -> Self {
        Self::new()
    }
}
