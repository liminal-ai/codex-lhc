//! Turn parts mid-turn compact — Codex host adapter (Story 5).
//!
//! At the settled post-sampling seam a migrating host invokes the certified
//! SDK `thread_view::mid_turn_compact`: the ordinary bounded prepare → install
//! compact, behind the four-fact host seam assertion (AC-7.4) and per-thread
//! mechanism exclusivity (AC-7.3). It splits the active turn into parts, or
//! settles/compacts, and never creates a synthetic boundary or continuation
//! turn.
//!
//! The single AC-7.3 amendment this host observes is **typed-only**: only a
//! typed `ForcedBoundaryThread` refusal routes a thread to the legacy
//! compact-continuation path. Every other refusal, storage error, or absent
//! result preserves the current body and retries at a later eligible seam — it
//! never falls open to native compaction while `Feature::LhcCapture` is on, and
//! it never asserts a false fact.

use std::path::Path;
use std::path::PathBuf;

use lhc::shared_tech::errors::ErrorCode;
use lhc::shared_tech::errors::OpResult;
use lhc::shared_tech::view::CompactReceipt;
use lhc::shared_tech::view::HostMetadata;
use lhc::shared_tech::view::StoredView;
use lhc::thread_view;
use lhc::thread_view::MidTurnCompactOptions;
use lhc::thread_view::MidTurnSeamAssertion;
use lhc::threads::ThreadRef;
use tracing::warn;

use crate::compact_continuation::thread_sqlite_path;
use lhc::compact_continuation::HostCompactOpts;

/// The four settled-seam facts, all asserted true. Story 5 only reaches this
/// entry point after the host has, at the settled post-sampling seam,
/// established a complete response, settled requested tools, a successful
/// capture flush, and position before the next provider request — so the
/// assertion is truthful by construction. An absent or false assertion would
/// be refused typed by the SDK the same as a caller error.
pub const SETTLED_MID_TURN_SEAM: MidTurnSeamAssertion = MidTurnSeamAssertion {
    model_response_complete: true,
    requested_tools_settled: true,
    capture_flushed: true,
    before_next_provider_request: true,
};

/// Outcome of one `thread_view::mid_turn_compact` invocation.
#[derive(Debug)]
pub enum MidTurnPartsOutcome {
    /// The SDK installed a serving view (parts split, settle, or whole
    /// compact). The host now materializes and rewrites the rollout to match.
    Installed(Box<CompactReceipt>),
    /// Typed `ForcedBoundaryThread` (AC-7.3): this thread already took the
    /// forced-boundary path before the migration and must keep using the
    /// legacy compact-continuation mechanism. The only path back to it.
    ForcedBoundaryThread,
    /// The durable active turn is not the turn the host bound to its current
    /// turn identity (AC-7.4 host side): `host_metadata.active_turn.turn_id`
    /// is absent or differs from `expected`. Compact was not invoked; the host
    /// keeps its current body and retries at a later eligible seam.
    ActiveTurnMismatch {
        expected: String,
        durable: Option<String>,
    },
    /// Any other typed refusal or storage error. No mutation happened; the host
    /// keeps its current body and retries at a later eligible seam. Never a
    /// license for native compaction.
    Refused { code: String, reason: String },
}

/// Inputs for a parts mid-turn compact.
pub struct MidTurnPartsRequest {
    pub thread_id: String,
    pub root: Option<PathBuf>,
    /// Durable LHC turn id capture bound to the host's current turn identity
    /// (`CaptureHandle::durable_turn_id`). The durable active turn read from
    /// `host_metadata` must equal it exactly, or compact is not invoked.
    pub active_turn_id: String,
    /// Band percentages / lower target for the bounded walk. `profile` and
    /// `params` are forwarded to the SDK entry; the seam is always the settled
    /// four-fact assertion above.
    pub compact: Option<HostCompactOpts>,
    /// Optional deterministic timestamp for the install (tests).
    pub created_at: Option<String>,
}

/// Invoke the certified parts mid-turn compact on the LHC thread.
///
/// The SDK enforces both directions of per-thread exclusivity: a thread that
/// ever served parts refuses the legacy path, and a thread that ever forced a
/// boundary refuses this one (returned here as [`MidTurnPartsOutcome::ForcedBoundaryThread`]).
pub async fn run_mid_turn_parts_compact(
    req: MidTurnPartsRequest,
) -> Result<MidTurnPartsOutcome, String> {
    let path = thread_sqlite_path(&req.thread_id, req.root.as_deref())
        .ok_or_else(|| "LHC root missing; cannot open mid-turn parts thread".to_string())?;
    if !path.exists() {
        return Err(format!(
            "LHC thread file missing for mid-turn parts compact: {}",
            path.display()
        ));
    }
    let ref_ = ThreadRef::file_path(path.to_string_lossy().into_owned());
    // AC-7.4 host side: verify the durable active turn is exactly the turn
    // bound to the host's current identity before freezing any input.
    let metadata = read_host_metadata_ref(ref_.clone()).await?;
    let durable = metadata.active_turn.map(|turn| turn.turn_id);
    if durable.as_deref() != Some(req.active_turn_id.as_str()) {
        warn!(
            expected = %req.active_turn_id,
            durable = durable.as_deref().unwrap_or("<none>"),
            "LHC mid-turn parts: durable active turn is not the current host turn; keeping current body"
        );
        return Ok(MidTurnPartsOutcome::ActiveTurnMismatch {
            expected: req.active_turn_id,
            durable,
        });
    }
    let (profile, params) = match req.compact {
        Some(opts) => (opts.profile, opts.params),
        None => (None, None),
    };
    let opts = MidTurnCompactOptions {
        seam: Some(SETTLED_MID_TURN_SEAM),
        profile,
        params,
        signal: None,
        created_at: req.created_at,
    };
    match thread_view::mid_turn_compact(ref_, opts).await {
        OpResult::Ok { value } => Ok(MidTurnPartsOutcome::Installed(Box::new(value))),
        OpResult::Err { error } => match error.code {
            ErrorCode::ForcedBoundaryThread => Ok(MidTurnPartsOutcome::ForcedBoundaryThread),
            other => {
                warn!(
                    code = other.as_str(),
                    reason = %error.reason,
                    "LHC mid-turn parts compact refused; keeping current body, retry at next seam"
                );
                Ok(MidTurnPartsOutcome::Refused {
                    code: other.as_str().to_string(),
                    reason: error.reason,
                })
            }
        },
    }
}

/// Deterministic, side-effect-free host metadata read (AC-7.1), used at the
/// seam to check the durable active turn's identity before invoking compact.
async fn read_host_metadata_ref(ref_: ThreadRef) -> Result<HostMetadata, String> {
    match thread_view::host_metadata(ref_).await {
        OpResult::Ok { value } => Ok(value),
        OpResult::Err { error } => Err(format!(
            "host_metadata {}: {}",
            error.code.as_str(),
            error.reason
        )),
    }
}

/// Read-only inspection of the installed serving view (parts evidence for
/// tests and canaries; never a decision input). `None` when no view exists.
pub async fn inspect_installed_view(
    thread_id: &str,
    root: Option<&Path>,
) -> Result<Option<StoredView>, String> {
    let path = thread_sqlite_path(thread_id, root)
        .ok_or_else(|| "LHC root missing; cannot inspect installed view".to_string())?;
    if !path.exists() {
        return Ok(None);
    }
    let ref_ = ThreadRef::file_path(path.to_string_lossy().into_owned());
    match thread_view::describe(ref_).await {
        OpResult::Ok { value } => Ok(value),
        OpResult::Err { error } => Err(format!(
            "describe {}: {}",
            error.code.as_str(),
            error.reason
        )),
    }
}

/// Whether an installed view serves any turn as parts (a part entry in the
/// arrangement). Inspection only.
pub fn view_serves_parts(view: &StoredView) -> bool {
    view.arrangement.iter().any(|entry| entry.part.is_some())
}
