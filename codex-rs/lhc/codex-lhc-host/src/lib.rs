//! LHC host adapter for Codex — capture + compact bridge.
//!
//! Chunk 1: capture. Chunk 2a: band-shape helpers. Chunk 2b: compact arm
//! produces a served body + archive compact marker (no body re-ingest).
//! Chunk 3: live cert.

mod band_shape;
mod body_validation;
mod capture;
mod compact_bridge;
mod compact_continuation;
mod gating;
mod idempotency;
mod inference;
mod install;
mod mapping;
mod materialize;
mod mid_turn_parts;
mod rollout_reconcile;
mod rollout_swap;
mod session;
mod tools;

#[cfg(test)]
#[path = "lim69_graft_tests.rs"]
mod lim69_graft_tests;

#[cfg(test)]
#[path = "cxs6_canary_tests.rs"]
mod cxs6_canary_tests;

pub use band_shape::BandShapeItem;
pub use band_shape::BandShapeReport;
pub use band_shape::DEFAULT_FULL_BAND_USER_TURNS;
pub use band_shape::band_shaped_history_from_events;
pub use band_shape::synthetic_minimal_band_history;
pub use body_validation::BODY_SIZE_SOURCE;
pub use body_validation::BodyDegradation;
pub use body_validation::BodyDegradationKind;
pub use body_validation::BodyValidationReport;
pub use body_validation::BodyValidationSpec;
pub use body_validation::DegradedBody;
pub use body_validation::GraftReport;
pub use body_validation::GraftSkip;
pub use body_validation::ProtectedPairExpectation;
pub use body_validation::TRUNCATION_MARKER;
pub use body_validation::capture_body_expectations;
pub use body_validation::client_call_id;
pub use body_validation::degradation_summary;
pub use body_validation::degrade_body_to_best_available;
pub use body_validation::graft_live_protected_pairs;
pub use body_validation::item_bytes_without_id;
pub use body_validation::item_without_host_provenance;
pub use body_validation::output_call_id;
pub use body_validation::validate_next_request_body;
pub use compact_bridge::COMPACT_MARKER_KEY_SEGMENT;
pub use compact_bridge::CompactMarker;
pub use compact_bridge::CoverageClass;
pub use compact_bridge::DerivedProvenance;
pub use compact_bridge::LhcBandPercentages;
pub use compact_bridge::LhcCompactResult;
pub use compact_bridge::LhcCompactUnavailable;
pub use compact_bridge::MaterializeSurfaces;
pub use compact_bridge::VIEW_MAP_SEAM_ID;
pub use compact_bridge::archive_tip_identity;
pub use compact_bridge::classify_coverage;
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
pub use compact_bridge::produce_lhc_compact_with_provenance_and_percentages;
pub use compact_bridge::read_materialize_surfaces;
pub use compact_bridge::unrepresentable_host_items_gap;
pub use compact_continuation::COMPACT_CONTINUATION_ACTOR;
pub use compact_continuation::CompactContinuationHysteresis;
pub use compact_continuation::DEFAULT_LOWER_TARGET_TOKENS;
pub use compact_continuation::MidTurnCompactContinuationOutcome;
pub use compact_continuation::MidTurnCompactContinuationRequest;
pub use compact_continuation::MidTurnRecoveryIdentity;
pub use compact_continuation::MidTurnTestHooks;
pub use compact_continuation::POST_MEASUREMENT_SOURCE;
pub use compact_continuation::build_host_facts;
pub use compact_continuation::compact_opts_with_band_percentages;
pub use compact_continuation::inspect_compact_continuation_attempt_intent;
pub use compact_continuation::inspect_compact_continuation_receipts;
pub use compact_continuation::inspect_compact_continuation_writer_claim;
pub use compact_continuation::inspect_compact_continuation_writer_owner;
pub use compact_continuation::inspect_has_compact_continuation_marker;
pub use compact_continuation::inspect_mid_turn_host_validation;
pub use compact_continuation::inspect_pending_compact_continuation_boundary;
pub use compact_continuation::mid_turn_seam;
pub use compact_continuation::missing_provider_usage_authority;
pub use compact_continuation::next_request_pressure;
pub use compact_continuation::record_mid_turn_host_validation;
pub use compact_continuation::resolve_mid_turn_recovery_identity;
pub use compact_continuation::run_mid_turn_compact_continuation;
#[cfg(feature = "test-util")]
pub use compact_continuation::seed_mid_turn_writer_claim_for_tests;
pub use compact_continuation::settled_mid_turn_seam;
pub use compact_continuation::test_compact_opts;
pub use compact_continuation::thread_sqlite_path;
pub use compact_continuation::token_usage_to_provider_usage_authority;
pub use compact_continuation::work_continuation_for_mid_turn;
pub use compact_continuation::work_continuation_from_history_tail;
pub use inference::LateBoundCallbacks;
pub use inference::LhcInferenceError;
pub use inference::inert_non_deriving_inference_callbacks;
pub use inference::lhc_inference_callbacks;
/// Re-export certified compact-continuation types for core MidTurn wiring.
pub use lhc::compact_continuation::CompactContinuationHostFacts;
pub use lhc::compact_continuation::HostCompactOpts;
pub use lhc::compact_continuation::HostValidationAck;
pub use lhc::compact_continuation::HostValidationStatus;
/// Re-export so core can pass live ModelClient-backed callbacks without a
/// direct `lhc` path dep.
pub use lhc::shared_tech::CompressDetailedTurnInput;
pub use lhc::shared_tech::InferenceCallbacks;
pub use lhc::shared_tech::InferenceResult;
pub use lhc::shared_tech::SmoothPromptInput;
pub use lhc::shared_tech::SummarizeChunkBriefInput;
pub use lhc::shared_tech::SummarizeToolResultInput;
pub use lhc::shared_tech::compact_continuation::CompactContinuationHostCapability;
pub use lhc::shared_tech::compact_continuation::CompactContinuationOutcomeKind;
pub use lhc::shared_tech::compact_continuation::CompactContinuationPolicy;
pub use lhc::shared_tech::compact_continuation::CompactContinuationRefuseCode;
pub use lhc::shared_tech::compact_continuation::CompactContinuationSeam;
pub use lhc::shared_tech::compact_continuation::CompactContinuationSkipCode;
pub use lhc::shared_tech::compact_continuation::PostMeasurementEstimate;
pub use lhc::shared_tech::compact_continuation::ProviderUsageAuthority;
pub use lhc::shared_tech::compact_continuation::WorkContinuation;
pub use lhc::shared_tech::compact_continuation::WriterClaim;
pub use mid_turn_parts::MidTurnPartsOutcome;
pub use mid_turn_parts::MidTurnPartsRequest;
pub use mid_turn_parts::SETTLED_MID_TURN_SEAM;
pub use mid_turn_parts::inspect_installed_view;
pub use mid_turn_parts::run_mid_turn_parts_compact;
pub use mid_turn_parts::view_serves_parts;
/// Return type of every [`InferenceCallbacks`] lane — lets hosts wrap the
/// callbacks (counting, tracing, delaying) without a direct `lhc` path dep.
pub type BoxInferenceFuture = lhc::shared_tech::derivation::BoxFuture<InferenceResult>;
pub use session::LhcSession;

pub use capture::CAPTURE_QUEUE_CAP;
pub use capture::CaptureHandle;
pub use capture::TurnBinding;
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
pub use install::LhcStepIndex;
pub use install::LhcTurnId;
pub use install::LiveRetrievalThread;
pub use install::RetrievalLifecycleError;
pub use install::install;
pub use install::install_with_provider_label;
pub use mapping::ACTOR_ASSISTANT;
pub use mapping::ACTOR_SYSTEM;
pub use mapping::ACTOR_TOOL;
pub use mapping::ACTOR_USER;
pub use mapping::HARNESS;
pub use mapping::MappedEvent;
pub use mapping::ModelIdentity;
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
pub use rollout_reconcile::CaptureFrontier;
pub use rollout_reconcile::CompactedRolloutIdentity;
pub use rollout_reconcile::ROLLOUT_REOPEN_RECEIPT_SCHEMA;
pub use rollout_reconcile::ReconcileOutcome;
pub use rollout_reconcile::ReopenReceiptOutcome;
pub use rollout_reconcile::RolloutFileClass;
pub use rollout_reconcile::RolloutReconcileTrigger;
pub use rollout_reconcile::RolloutReopenFailureReceipt;
pub use rollout_reconcile::classify_rollout_vs_thread;
pub use rollout_reconcile::compacted_record_count;
pub use rollout_reconcile::compacted_rollout_identity;
pub use rollout_reconcile::consume_reopen_failure_receipt;
pub use rollout_reconcile::file_boundary_compact_point;
pub use rollout_reconcile::host_validation_reload_warning;
pub use rollout_reconcile::is_native_append_polluted;
pub use rollout_reconcile::materialize_thread_rollout_items;
pub use rollout_reconcile::read_capture_frontier;
pub use rollout_reconcile::read_rollout_reopen_failure_receipt;
pub use rollout_reconcile::read_thread_compact_point;
pub use rollout_reconcile::reconcile_rollout_at_path;
pub use rollout_reconcile::regenerate_rollout_from_thread;
pub use rollout_reconcile::rollout_reopen_receipt_path;
pub use rollout_reconcile::write_rollout_reopen_failure_receipt;
pub use rollout_swap::SwapFailpoint;
pub use rollout_swap::SwapGenerations;
pub use rollout_swap::SwapPaths;
pub use rollout_swap::SwapReconciliation;
pub use rollout_swap::SwapState;
pub use rollout_swap::atomic_rewrite_rollout;
pub use rollout_swap::classify_swap_state;
pub use rollout_swap::history_from_materialized_items;
pub use rollout_swap::model_context_token_estimate_from_rollout_items;
pub use rollout_swap::parse_rollout_items;
pub use rollout_swap::reconcile_interrupted_swap;
pub use rollout_swap::strict_read_generation;
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
