//! LHC host adapter for Codex — Chunk 1 capture.
//!
//! Chunk 1 delivers capture only — no compaction, no user-facing benefit.
//! Chunks 2–3 remain (rebuild/compact bridge + live cert).

mod capture;
mod gating;
mod idempotency;
mod install;
mod mapping;
mod session;

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
pub use mapping::map_item;
pub use mapping::map_model_or_thinking_change;
pub use mapping::map_runtime_note;
pub use mapping::map_turn_end;
pub use session::encode_thread_id_for_path;
pub use session::thread_file_path;

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
#[cfg(any(test, feature = "test-util"))]
pub use session::LhcSession;

#[cfg(test)]
mod tests {
    #[test]
    fn vendored_port_links_with_js_number_parity() {
        assert_eq!(super::lhc_port_linked(), r#"{"linked":1e+21}"#);
    }
}
