//! LHC host adapter for Codex — capture + compact bridge.
//!
//! Chunk 1: capture. Chunk 2a: band-shape helpers. Chunk 2b: compact arm
//! produces a served body + archive compact marker (no body re-ingest).
//! Chunk 3: live cert.

mod band_shape;
mod capture;
mod compact_bridge;
mod gating;
mod idempotency;
mod inference;
mod install;
mod mapping;
mod materialize;
mod rollout_swap;
mod session;

pub use band_shape::BandShapeItem;
pub use band_shape::BandShapeReport;
pub use band_shape::DEFAULT_FULL_BAND_USER_TURNS;
pub use band_shape::band_shaped_history_from_events;
pub use band_shape::synthetic_minimal_band_history;
pub use compact_bridge::COMPACT_MARKER_KEY_SEGMENT;
pub use compact_bridge::CompactMarker;
pub use compact_bridge::DerivedProvenance;
pub use compact_bridge::LhcCompactResult;
pub use compact_bridge::LhcCompactUnavailable;
pub use compact_bridge::MaterializeSurfaces;
pub use compact_bridge::VIEW_MAP_SEAM_ID;
pub use compact_bridge::archive_tip_identity;
pub use compact_bridge::commit_compact_marker;
pub use compact_bridge::content_identity_digest;
pub use compact_bridge::derived_ids_from_archive;
pub use compact_bridge::estimate_response_items_tokens;
pub use compact_bridge::host_history_coverage_gap;
pub use compact_bridge::host_history_coverage_gap_with_derived;
pub use compact_bridge::host_history_coverage_gap_with_provenance;
pub use compact_bridge::host_items_missing_from_archive;
pub use compact_bridge::host_items_missing_from_archive_with_derived;
pub use compact_bridge::host_items_missing_from_archive_with_provenance;
pub use compact_bridge::import_host_items_into_archive;
pub use compact_bridge::is_compact_marker_idempotency_key;
pub use compact_bridge::llm_request_context_to_response_items;
pub use compact_bridge::produce_lhc_compact;
pub use compact_bridge::produce_lhc_compact_deterministic;
pub use compact_bridge::produce_lhc_compact_with_derived;
pub use compact_bridge::produce_lhc_compact_with_provenance;
pub use compact_bridge::read_materialize_surfaces;
pub use inference::LateBoundCallbacks;
pub use inference::LhcInferenceError;
pub use inference::lhc_inference_callbacks;
/// Re-export so core can pass live ModelClient-backed callbacks without a
/// direct `lhc` path dep.
pub use lhc::shared_tech::CompressDetailedTurnInput;
pub use lhc::shared_tech::InferenceCallbacks;
pub use lhc::shared_tech::InferenceResult;
pub use lhc::shared_tech::SmoothPromptInput;
pub use lhc::shared_tech::SummarizeChunkBriefInput;
pub use lhc::shared_tech::SummarizeToolResultInput;
/// Return type of every [`InferenceCallbacks`] lane — lets hosts wrap the
/// callbacks (counting, tracing, delaying) without a direct `lhc` path dep.
pub type BoxInferenceFuture = lhc::shared_tech::derivation::BoxFuture<InferenceResult>;
pub use session::LhcSession;

pub use capture::CAPTURE_QUEUE_CAP;
pub use capture::CaptureHandle;
pub use capture::spawn_capture;
pub use gating::lhc_root;
pub use idempotency::OccurrenceTracker;
pub use idempotency::encode_thread_id;
pub use idempotency::item_digest;
pub use idempotency::item_event_key;
pub use idempotency::item_stable_id;
pub use idempotency::model_change_key;
pub use idempotency::seed_occurrence_from_keys;
pub use idempotency::thinking_level_change_key;
pub use idempotency::turn_end_key;
pub use install::LhcCaptureSlot;
pub use install::LhcTurnId;
pub use install::install;
pub use mapping::ACTOR_ASSISTANT;
pub use mapping::ACTOR_SYSTEM;
pub use mapping::ACTOR_TOOL;
pub use mapping::ACTOR_USER;
pub use mapping::HARNESS;
pub use mapping::MappedEvent;
pub use mapping::TurnEndFacts;
pub use mapping::attach_provider_usage;
pub use mapping::map_item;
pub use mapping::map_model_or_thinking_change;
pub use mapping::map_runtime_note;
pub use mapping::map_turn_end;
pub use mapping::token_usage_to_provider_usage;
pub use mapping::unix_secs_to_iso;
pub use materialize::CAPTURE_GAPS;
pub use materialize::CompactBoundaryMeta;
pub use materialize::MaterializeInput;
pub use materialize::MaterializeResult;
pub use materialize::boundary_completeness_error;
pub use materialize::iso_to_unix_secs;
pub use materialize::materialize_rollout;
pub use materialize::model_stream_response_item_count;
pub use rollout_swap::SwapFailpoint;
pub use rollout_swap::SwapPaths;
pub use rollout_swap::atomic_rewrite_rollout;
pub use rollout_swap::history_from_materialized_items;
pub use rollout_swap::parse_rollout_items;
pub use session::encode_thread_id_for_path;
pub use session::thread_file_path;

#[cfg(any(test, feature = "test-util"))]
pub use rollout_swap::SwapFailpointGuard;
#[cfg(any(test, feature = "test-util"))]
pub use rollout_swap::clear_swap_failpoint;
#[cfg(any(test, feature = "test-util"))]
pub use rollout_swap::set_swap_failpoint;

/// Linkage proof through a real, behavior-bearing port export.
pub fn lhc_port_linked() -> String {
    lhc::shared_tech::js_json::js_json_stringify(&serde_json::json!({"linked": 1e21}))
}

/// Re-export event records for host-side certification assertions.
pub use lhc::intake_stream::EventRecord;
pub use lhc::sdk::init_lhc;

#[cfg(any(test, feature = "test-util"))]
pub use gating::env_lock;
#[cfg(any(test, feature = "test-util"))]
pub use install::install_with_root;
#[cfg(any(test, feature = "test-util"))]
pub use install::install_with_root_and_labels;
#[cfg(any(test, feature = "test-util"))]
pub use install::wait_for_handle;

#[cfg(test)]
mod tests {
    #[test]
    fn vendored_port_links_with_js_number_parity() {
        assert_eq!(super::lhc_port_linked(), r#"{"linked":1e+21}"#);
    }
}
