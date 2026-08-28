//! Registry wiring for the Codex LHC capture extension.
//!
//! Feature gating is runtime: when `lhc_enabled(config)` is false at thread
//! start, no open is scheduled and raw-item callbacks are pure no-ops.
//!
//! Open is **lazy / off the critical path** (F17): `on_thread_start` only
//! schedules a background open; the user's Session construction is not blocked
//! on SQLite. Items that arrive before the handle is ready are **buffered**
//! (H2) and flushed on open — bounded with the same degraded policy on
//! overflow. Prefer a complete opening over silent loss of the first prompt.
//!
//! Model / thinking-level changes ride the free `ConfigContributor` fan-out
//! (`on_config_changed` with previous/new Config snapshots) — no new core
//! touchpoint (G1 / F15).

use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_extension_api::ConfigContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::RawItemContributor;
use codex_extension_api::RawItemInput;
use codex_extension_api::RawItemProvenance;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadResumeInput;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_extension_api::TokenUsageContributor;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnAbortReason;
use tracing::debug;
use tracing::error;
use tracing::warn;

use lhc::shared_tech::InferenceCallbacks;

use crate::capture::CAPTURE_QUEUE_CAP;
use crate::capture::CaptureHandle;
use crate::capture::CaptureShutdownFailure;
use crate::capture::CaptureShutdownResult;
use crate::capture::spawn_capture_with_identity;
use crate::gating::lhc_root;
use crate::mapping::ModelIdentity;
use crate::mapping::TurnEndFacts;
use crate::mapping::unix_secs_to_iso;

/// Cap a set to `max` entries by dropping arbitrary extras; **logs** the drop (H3).
/// Only used on **superseded** provenance — never on the current body (L3).
fn cap_hashset_with_log(set: &mut HashSet<String>, max: usize, label: &str) {
    if set.len() <= max {
        return;
    }
    let drop_n = set.len() - max;
    let overflow: Vec<String> = set.iter().take(drop_n).cloned().collect();
    for k in &overflow {
        set.remove(k);
    }
    warn!(
        dropped = drop_n,
        retained = set.len(),
        cap = max,
        label,
        "LHC: superseded derived set capped — dropped historical entries only"
    );
}

/// Process-local cap on **superseded** (non-current) derived provenance (H3/L3).
/// The current body's ids/digests are pinned and never subject to this cap.
///
/// There is deliberately no test override. There used to be a mutable global
/// one, and it was a shared-state smell that duly bit: two `l3_*` tests set it
/// concurrently and the suite failed only under parallelism, passing in
/// isolation. The tests now drive the real constant — provenance ids are just
/// strings in a set, so exercising the true cap costs nothing.
fn session_derived_cap() -> usize {
    SESSION_DERIVED_CAP
}

/// Bound on the pre-open buffer (same order as the capture queue).
// One below the channel cap: normal traffic gets CAPTURE_QUEUE_CAP - 1
// usable slots (one is reserved for the truncation note), so an exactly-full
// pre-open buffer must fit the same budget or its last replayed command
// would latch persist_full.
const PRE_OPEN_CAP: usize = CAPTURE_QUEUE_CAP - 1;

/// Pending work that arrived before the capture handle was ready (H2).
enum PendingCmd {
    Persist {
        item: ResponseItem,
        provenance: RawItemProvenance,
        step_index: Option<i64>,
        /// In-run steer assertion for a user prompt (turn parts, Flow 7).
        steer: bool,
    },
    ModelOrThinkingChange {
        previous_model: String,
        new_model: String,
        previous_level: String,
        new_level: String,
    },
    ProviderUsage {
        usage: codex_protocol::protocol::TokenUsage,
    },
    SetIdentity {
        identity: ModelIdentity,
    },
    TurnEnd {
        turn_id: String,
        reason: String,
        facts: TurnEndFacts,
    },
    BindTurn {
        host_turn_id: String,
    },
}

/// Cap on **superseded** (historical) derived provenance retained across
/// write-backs (H3). Current-body ids/digests are **never** capped (L3).
pub const SESSION_DERIVED_CAP: usize = 512;

/// The capture slot's whole lifecycle, in one watchable value (LIM-134).
///
/// Every transition that can end a readiness wait is published through the
/// slot's `watch` channel, so waiters cannot miss one. `Opening` is the only
/// non-terminal state; `Failed` and `Stopped` are terminal and suppress any
/// later `Ready` publication from the dedicated open thread.
///
/// There is deliberately no persistence here: this is process-local runtime
/// state, not a durable readiness record.
#[derive(Clone)]
pub enum CaptureState {
    /// No handle published yet — the asynchronous open is in flight (or has
    /// not been scheduled).
    Opening,
    /// The background open published a live handle.
    Ready(CaptureHandle),
    /// The background open failed permanently. The reason is stable and
    /// visible to compact / retrieval callers.
    Failed(String),
    /// Thread stop ran, or the shutdown bound expired while still `Opening`.
    Stopped,
}

impl CaptureState {
    /// Whether the slot has left `Opening` (a readiness wait may end).
    fn is_settled(&self) -> bool {
        !matches!(self, Self::Opening)
    }
}

impl std::fmt::Debug for CaptureState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Opening => f.write_str("Opening"),
            // CaptureHandle is not Debug; the thread id is the useful fact.
            Self::Ready(handle) => write!(f, "Ready({})", handle.thread_id()),
            Self::Failed(reason) => write!(f, "Failed({reason})"),
            Self::Stopped => f.write_str("Stopped"),
        }
    }
}

/// Stable visible reasons published with [`CaptureState::Failed`].
pub const CAPTURE_OPEN_RUNTIME_UNAVAILABLE: &str = "capture open runtime unavailable";
pub const CAPTURE_OPEN_FAILED: &str = "capture open failed";
pub const CAPTURE_OPEN_THREAD_UNAVAILABLE: &str = "capture open thread unavailable";
pub const CAPTURE_OPEN_ABANDONED: &str = "capture open abandoned before settling";

/// Why resolving a live retrieval thread from the capture slot failed.
///
/// Validation failures never reach this path — they refuse before any SDK
/// open. Lifecycle failures also write zero impression rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalLifecycleError {
    /// Slot exists but the async open has not published a handle yet.
    NotOpen,
    /// Background open failed permanently.
    OpenFailed,
    /// Thread stop already ran; capture worker is shutting down / gone.
    Shutdown,
}

impl RetrievalLifecycleError {
    pub fn message(self) -> &'static str {
        match self {
            Self::NotOpen => "LHC thread is not open yet (capture still starting)",
            Self::OpenFailed => "LHC capture failed to open; retrieval unavailable",
            Self::Shutdown => "LHC capture has shut down; retrieval unavailable",
        }
    }
}

/// Live thread identity for retrieval: thread id + LHC root (file path).
#[derive(Debug, Clone)]
pub struct LiveRetrievalThread {
    pub thread_id: String,
    pub root: PathBuf,
}

/// Per-thread capture slot: handle is filled asynchronously after start.
/// Items arriving before open are buffered and flushed when the handle lands.
pub struct LhcCaptureSlot {
    /// The one coherent, watchable lifecycle (LIM-134). Replaces the former
    /// `handle` / `opening` / `open_failed` / `stopped` spread, which allowed
    /// a required compact to observe "no handle" and allowed a late `Ready`
    /// publication after stop.
    state: tokio::sync::watch::Sender<CaptureState>,
    pending: Mutex<VecDeque<PendingCmd>>,
    pending_overflow: AtomicBool,
    pending_dropped: AtomicU64,
    /// One-shot guard so `schedule_open` spawns at most one open thread.
    open_scheduled: AtomicBool,
    /// Pinned provenance for the **current** installed body (+ durable reseed).
    /// Never subject to [`SESSION_DERIVED_CAP`] eviction (L3).
    pinned_ids: Mutex<HashSet<String>>,
    pinned_digests: Mutex<HashSet<String>>,
    /// Superseded by a later compact — may be capped (H3/L3).
    superseded_ids: Mutex<HashSet<String>>,
    superseded_digests: Mutex<HashSet<String>>,
    /// Late binding for the **capture session's** derivation callbacks.
    ///
    /// Under `SdkMode::Background` LHC's scheduler derives on the capture
    /// session, so this is what lands in the durable record. J1 applies as hard
    /// as it does at compact time: only the host's production callbacks are
    /// ever installed here — never the deterministic ones, which would bake
    /// canned text into the record and serve it as real derivation.
    derivation_callbacks: crate::inference::LateBoundCallbacks,
    /// MidTurn compact-continuation hysteresis (no-reduction treadmill guard).
    mid_turn_hysteresis: Mutex<crate::compact_continuation::CompactContinuationHysteresis>,
    /// Optional test-only compact opts (small lower bounds) for MidTurn offline
    /// evidence. Production leaves this unset so the SDK profile policy applies.
    #[cfg(any(test, feature = "test-util"))]
    mid_turn_test_compact: Mutex<Option<lhc::compact_continuation::HostCompactOpts>>,
    /// Optional test-only upper trigger override (tokens). Production leaves
    /// this unset so Codex model/window policy applies.
    #[cfg(any(test, feature = "test-util"))]
    mid_turn_test_upper_trigger: Mutex<Option<i64>>,
    /// Optional test-only MidTurn fault hooks (degraded/invalid residual paths).
    /// Production leaves this unset.
    #[cfg(any(test, feature = "test-util"))]
    mid_turn_test_hooks: Mutex<Option<crate::compact_continuation::MidTurnTestHooks>>,
    /// Optional test-only safe-runway threshold override (tokens). Production
    /// leaves this unset so the Codex auto-compact scope limit applies.
    #[cfg(any(test, feature = "test-util"))]
    mid_turn_test_safe_runway: Mutex<Option<i64>>,
    /// Test-only: force the LIM-67 host full-body validation to fail after a
    /// successful core install (negative-path evidence). Production unset.
    #[cfg(any(test, feature = "test-util"))]
    mid_turn_test_force_body_validation_fail: std::sync::atomic::AtomicBool,
    /// Test-only: force the R11 validation-ACK write to fail after install
    /// (arm-level warn-and-continue evidence). Production unset.
    #[cfg(any(test, feature = "test-util"))]
    mid_turn_test_force_validation_ack_write_fail: std::sync::atomic::AtomicBool,
}

impl LhcCaptureSlot {
    /// Construct an empty slot (tests / internal). Production inserts via
    /// `on_thread_start`.
    pub(crate) fn new() -> Self {
        Self {
            state: tokio::sync::watch::Sender::new(CaptureState::Opening),
            pending: Mutex::new(VecDeque::new()),
            pending_overflow: AtomicBool::new(false),
            pending_dropped: AtomicU64::new(0),
            open_scheduled: AtomicBool::new(false),
            pinned_ids: Mutex::new(HashSet::new()),
            pinned_digests: Mutex::new(HashSet::new()),
            superseded_ids: Mutex::new(HashSet::new()),
            superseded_digests: Mutex::new(HashSet::new()),
            derivation_callbacks: crate::inference::LateBoundCallbacks::new(),
            mid_turn_hysteresis: Mutex::new(
                crate::compact_continuation::CompactContinuationHysteresis::default(),
            ),
            #[cfg(any(test, feature = "test-util"))]
            mid_turn_test_compact: Mutex::new(None),
            #[cfg(any(test, feature = "test-util"))]
            mid_turn_test_upper_trigger: Mutex::new(None),
            #[cfg(any(test, feature = "test-util"))]
            mid_turn_test_hooks: Mutex::new(None),
            #[cfg(any(test, feature = "test-util"))]
            mid_turn_test_safe_runway: Mutex::new(None),
            #[cfg(any(test, feature = "test-util"))]
            mid_turn_test_force_body_validation_fail: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "test-util"))]
            mid_turn_test_force_validation_ack_write_fail: std::sync::atomic::AtomicBool::new(
                false,
            ),
        }
    }

    /// Clear MidTurn no-reduction hysteresis after any successful LHC install
    /// (PreTurn / manual / MidTurn reduction). Prevents a stale margin band
    /// from suppressing a warranted re-attempt after non-MidTurn relief.
    pub fn clear_mid_turn_hysteresis(&self, attempt_id: &str, pressure: i64, outcome: &str) {
        self.mid_turn_hysteresis
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear(attempt_id, pressure, outcome);
    }

    /// Snapshot MidTurn hysteresis (no-reduction treadmill guard).
    pub fn mid_turn_hysteresis(
        &self,
    ) -> crate::compact_continuation::CompactContinuationHysteresis {
        self.mid_turn_hysteresis
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Record MidTurn hysteresis after a compact-continuation attempt.
    pub fn record_mid_turn_hysteresis(
        &self,
        attempt_id: &str,
        pressure: i64,
        reduced: bool,
        outcome: &str,
    ) {
        self.mid_turn_hysteresis
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(attempt_id, pressure, reduced, outcome);
    }

    /// Install test-only compact opts for MidTurn (offline evidence only).
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_mid_turn_test_compact(
        &self,
        opts: Option<lhc::compact_continuation::HostCompactOpts>,
    ) {
        *self
            .mid_turn_test_compact
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = opts;
    }

    /// Take MidTurn test compact opts without clearing (clone).
    #[cfg(any(test, feature = "test-util"))]
    pub fn mid_turn_test_compact(&self) -> Option<lhc::compact_continuation::HostCompactOpts> {
        self.mid_turn_test_compact
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Install test-only upper trigger for MidTurn (offline evidence only).
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_mid_turn_test_upper_trigger(&self, upper: Option<i64>) {
        *self
            .mid_turn_test_upper_trigger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = upper;
    }

    /// Take MidTurn test upper trigger without clearing.
    #[cfg(any(test, feature = "test-util"))]
    pub fn mid_turn_test_upper_trigger(&self) -> Option<i64> {
        *self
            .mid_turn_test_upper_trigger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Install test-only MidTurn fault hooks (degraded / invalid residual only).
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_mid_turn_test_hooks(
        &self,
        hooks: Option<crate::compact_continuation::MidTurnTestHooks>,
    ) {
        *self
            .mid_turn_test_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hooks;
    }

    /// Take MidTurn test fault hooks without clearing.
    #[cfg(any(test, feature = "test-util"))]
    pub fn mid_turn_test_hooks(&self) -> Option<crate::compact_continuation::MidTurnTestHooks> {
        self.mid_turn_test_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Install test-only safe-runway threshold for MidTurn (offline evidence).
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_mid_turn_test_safe_runway(&self, threshold: Option<i64>) {
        *self
            .mid_turn_test_safe_runway
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = threshold;
    }

    /// Take MidTurn test safe-runway threshold without clearing.
    #[cfg(any(test, feature = "test-util"))]
    pub fn mid_turn_test_safe_runway(&self) -> Option<i64> {
        *self
            .mid_turn_test_safe_runway
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Test-only: force LIM-67 host body validation to fail (negative path).
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_mid_turn_test_force_body_validation_fail(&self, fail: bool) {
        self.mid_turn_test_force_body_validation_fail
            .store(fail, Ordering::SeqCst);
    }

    /// Test-only: read the forced body-validation failure flag.
    #[cfg(any(test, feature = "test-util"))]
    pub fn mid_turn_test_force_body_validation_fail(&self) -> bool {
        self.mid_turn_test_force_body_validation_fail
            .load(Ordering::SeqCst)
    }

    /// Test-only: force the R11 validation-ACK write to fail (arm-level
    /// warn-and-continue evidence).
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_mid_turn_test_force_validation_ack_write_fail(&self, fail: bool) {
        self.mid_turn_test_force_validation_ack_write_fail
            .store(fail, Ordering::SeqCst);
    }

    /// Test-only: read the forced ACK-write failure flag.
    #[cfg(any(test, feature = "test-util"))]
    pub fn mid_turn_test_force_validation_ack_write_fail(&self) -> bool {
        self.mid_turn_test_force_validation_ack_write_fail
            .load(Ordering::SeqCst)
    }

    /// Current lifecycle state (LIM-134).
    pub fn state(&self) -> CaptureState {
        self.state.borrow().clone()
    }

    /// Publish `Ready` — **only** from `Opening`. Returns false when the slot
    /// already left `Opening` (stop won the race), in which case the caller
    /// must neither replay nor use the handle.
    fn publish_ready(&self, handle: CaptureHandle) -> bool {
        let mut published = false;
        self.state.send_if_modified(|state| {
            if state.is_settled() {
                return false;
            }
            *state = CaptureState::Ready(handle.clone());
            published = true;
            true
        });
        published
    }

    /// Publish `Failed(reason)` — only from `Opening`.
    fn publish_failed(&self, reason: impl Into<String>) -> bool {
        let reason = reason.into();
        let mut published = false;
        self.state.send_if_modified(|state| {
            if state.is_settled() {
                return false;
            }
            *state = CaptureState::Failed(reason.clone());
            published = true;
            true
        });
        published
    }

    /// Publish `Stopped` from any state. Idempotent; always leaves the slot
    /// terminal so a later `Ready` publication is suppressed.
    fn publish_stopped(&self) {
        self.state.send_if_modified(|state| {
            if matches!(state, CaptureState::Stopped) {
                return false;
            }
            *state = CaptureState::Stopped;
            true
        });
    }

    /// Transition to `Stopped` and drop the pre-open buffer exactly once with
    /// one loud warning. Taken under the pending lock so a concurrent
    /// `set_and_flush` cannot slip a replay past the transition.
    fn stop_and_drop_pending(&self) -> usize {
        let dropped = {
            let mut q = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.publish_stopped();
            std::mem::take(&mut *q).len()
        };
        if dropped > 0 {
            error!(
                dropped,
                telemetry_event = "lhc.capture_open_abandoned_at_stop",
                "LHC: capture open did not complete before thread stop; dropping buffered pre-open commands"
            );
        }
        dropped
    }

    /// Wait until the slot leaves `Opening`, or `bound` expires.
    ///
    /// The subscribe happens here, before the first read of the value, and the
    /// state rides a `watch` channel — so a transition published either side of
    /// the subscribe still ends the wait.
    pub async fn await_settled(&self, bound: std::time::Duration) -> Option<CaptureState> {
        self.wait_settled(self.state.subscribe(), bound).await
    }

    /// Subscribe now, wait later. Tests use this to make "the waiter cannot
    /// miss the transition" structural rather than a scheduling race.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn subscribe_lifecycle(&self) -> tokio::sync::watch::Receiver<CaptureState> {
        self.state.subscribe()
    }

    async fn wait_settled(
        &self,
        mut rx: tokio::sync::watch::Receiver<CaptureState>,
        bound: std::time::Duration,
    ) -> Option<CaptureState> {
        tokio::time::timeout(bound, async move {
            loop {
                {
                    let state = rx.borrow_and_update().clone();
                    if state.is_settled() {
                        return state;
                    }
                }
                if rx.changed().await.is_err() {
                    // Slot dropped out from under the waiter; nothing can open.
                    return CaptureState::Stopped;
                }
            }
        })
        .await
        .ok()
    }

    /// Wait on a receiver taken earlier via [`Self::subscribe_lifecycle`].
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) async fn await_settled_on(
        &self,
        rx: tokio::sync::watch::Receiver<CaptureState>,
        bound: std::time::Duration,
    ) -> Option<CaptureState> {
        self.wait_settled(rx, bound).await
    }

    /// Test-only: live receiver count on the existing lifecycle watch.
    ///
    /// Production [`Self::await_settled`] subscribes before waiting. An
    /// increase of one after `TurnStarted` is structural proof that waiter
    /// armed. No extra field or channel.
    #[cfg(any(test, feature = "test-util"))]
    pub fn readiness_waiter_count_for_test(&self) -> usize {
        self.state.receiver_count()
    }

    /// Test-only: publish `Ready` (with the ordered pre-open replay) exactly
    /// as the background open thread would. Returns whether it published.
    #[cfg(any(test, feature = "test-util"))]
    pub fn publish_ready_for_test(&self, handle: CaptureHandle) -> bool {
        self.set_and_flush(handle)
    }

    /// Test-only: publish a permanent open failure.
    #[cfg(any(test, feature = "test-util"))]
    pub fn publish_failed_for_test(&self, reason: &str) -> bool {
        self.publish_failed(reason)
    }

    /// Test-only: publish the terminal stopped state.
    #[cfg(any(test, feature = "test-util"))]
    pub fn publish_stopped_for_test(&self) {
        self.publish_stopped();
    }

    /// Resolve the live capture thread for retrieval tools.
    ///
    /// Does not open SQLite — only inspects slot lifecycle state and the
    /// published handle's thread id / root. Callers open via
    /// [`crate::session::thread_file_path`] + `ThreadRef::file_path`.
    pub fn resolve_for_retrieval(&self) -> Result<LiveRetrievalThread, RetrievalLifecycleError> {
        let handle = match self.state() {
            CaptureState::Ready(handle) => handle,
            CaptureState::Opening => return Err(RetrievalLifecycleError::NotOpen),
            CaptureState::Failed(_) => return Err(RetrievalLifecycleError::OpenFailed),
            CaptureState::Stopped => return Err(RetrievalLifecycleError::Shutdown),
        };
        let root = handle
            .root()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(lhc_root);
        Ok(LiveRetrievalThread {
            thread_id: handle.thread_id().to_string(),
            root,
        })
    }

    /// Install the production derivation callbacks the capture session's
    /// background scheduler derives with.
    ///
    /// The host resolves these the same way the compact arm does; passing
    /// deterministic callbacks here in production would silently degrade the
    /// durable record (see the field docs).
    pub fn set_derivation_callbacks(&self, callbacks: InferenceCallbacks) {
        self.derivation_callbacks.seed(callbacks);
    }

    /// Whether background derivation callbacks have been seeded.
    /// Hosts use this to resolve the (non-trivial) callbacks only once.
    pub fn has_derivation_callbacks(&self) -> bool {
        self.derivation_callbacks.is_seeded()
    }

    /// Current capture handle, if the background open has completed.
    pub fn get(&self) -> Option<CaptureHandle> {
        match self.state() {
            CaptureState::Ready(handle) => Some(handle),
            CaptureState::Opening | CaptureState::Failed(_) | CaptureState::Stopped => None,
        }
    }

    /// Record derived provenance after write-back (H1/L3). Previous current
    /// body moves to superseded (may be capped); the new body is pinned and
    /// never evicted.
    pub fn mark_derived_after_writeback(
        &self,
        ids: impl IntoIterator<Item = String>,
        digests: impl IntoIterator<Item = String>,
    ) -> Result<(), String> {
        let ids: Vec<String> = ids.into_iter().filter(|s| !s.is_empty()).collect();
        let digests: Vec<String> = digests.into_iter().filter(|s| !s.is_empty()).collect();
        if ids.is_empty() && digests.is_empty() {
            return Err("mark_derived_after_writeback: empty ids and digests".into());
        }
        let cap = session_derived_cap();
        {
            let mut pinned = self
                .pinned_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut superseded = self
                .superseded_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Prior current body is now historical.
            superseded.extend(pinned.drain());
            cap_hashset_with_log(&mut superseded, cap, "superseded_derived_ids");
            for id in ids {
                pinned.insert(id);
            }
        }
        {
            let mut pinned = self
                .pinned_digests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut superseded = self
                .superseded_digests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            superseded.extend(pinned.drain());
            cap_hashset_with_log(&mut superseded, cap, "superseded_derived_digests");
            for d in digests {
                pinned.insert(d);
            }
        }
        Ok(())
    }

    /// Merge durable CompactedItem provenance into the **pinned** set (L3).
    /// Durable is source of truth — never skipped because the cache is warm.
    pub fn ensure_derived_from_durable(
        &self,
        ids: impl IntoIterator<Item = String>,
        digests: impl IntoIterator<Item = String>,
    ) {
        let mut pinned_ids = self
            .pinned_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in ids.into_iter().filter(|s| !s.is_empty()) {
            pinned_ids.insert(id);
        }
        let mut pinned_digests = self
            .pinned_digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for d in digests.into_iter().filter(|s| !s.is_empty()) {
            pinned_digests.insert(d);
        }
    }

    /// Session-local derived host ids (pinned ∪ superseded) for import exclusion.
    pub fn derived_ids(&self) -> HashSet<String> {
        let pinned = self
            .pinned_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let superseded = self
            .superseded_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pinned.iter().chain(superseded.iter()).cloned().collect()
    }

    /// Session-local derived content digests (pinned ∪ superseded).
    pub fn derived_digests(&self) -> HashSet<String> {
        let pinned = self
            .pinned_digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let superseded = self
            .superseded_digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pinned.iter().chain(superseded.iter()).cloned().collect()
    }

    /// Test/crash simulation: drop process-local derived provenance (I2).
    #[cfg(any(test, feature = "test-util"))]
    pub fn clear_derived_for_test(&self) {
        self.pinned_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.pinned_digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.superseded_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.superseded_digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Install the handle and flush any pre-open buffer (H2).
    ///
    /// Handoff is loss-free and ordered: buffered commands replay in drain
    /// loops (a command that races into the queue mid-replay is caught by the
    /// next pass), and the handle publishes under the SAME pending lock that
    /// `buffer_or_handle` re-checks it under — so no command can slip into a
    /// drained queue after the final pass, and no direct send can overtake a
    /// buffered one. Lock order (pending → state) matches `buffer_or_handle`.
    ///
    /// LIM-134: every pass re-reads the lifecycle under the pending lock. Once
    /// the slot has left `Opening` (thread stop abandoned this open), neither
    /// publication nor further replay happens and the caller learns so.
    /// Returns whether `Ready` was published.
    fn set_and_flush(&self, handle: CaptureHandle) -> bool {
        // Overflow latches degraded BEFORE any replay (first drain pass
        // rechecks under the lock): if commands were dropped — possibly an
        // identity update — replaying survivors could tag output with stale
        // identity, so persists must already be no-ops.
        let mut publishable = Some(handle);
        loop {
            let pending = {
                let mut q = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if self.state.borrow().is_settled() {
                    return false;
                }
                // Recheck under the drain lock every pass: a producer can
                // overflow DURING replay, after the entry check — the handle
                // must never publish healthy over dropped commands.
                if self.pending_overflow.load(Ordering::SeqCst)
                    && let Some(h) = publishable.as_ref()
                {
                    h.latch_degraded("pre_open_overflow");
                }
                if q.is_empty() {
                    // Final pass: publish while still holding the pending
                    // lock — concurrent buffer_or_handle callers block on
                    // this lock, then see the handle on their re-check.
                    let Some(live) = publishable.take() else {
                        return false;
                    };
                    return self.publish_ready(live);
                }
                std::mem::take(&mut *q)
            };
            // Present until the final (empty-queue) pass publishes it.
            let Some(live) = publishable.as_ref() else {
                return false;
            };
            self.replay(live, pending);
        }
    }

    /// Replay drained pre-open commands onto the live handle, in order.
    fn replay(&self, handle: &CaptureHandle, pending: std::collections::VecDeque<PendingCmd>) {
        for cmd in pending {
            match cmd {
                PendingCmd::Persist {
                    item,
                    provenance,
                    step_index,
                    steer,
                } => {
                    if steer {
                        handle.persist_steer_prompt(&item);
                    } else {
                        handle.persist(&item, provenance, step_index);
                    }
                }
                PendingCmd::ModelOrThinkingChange {
                    previous_model,
                    new_model,
                    previous_level,
                    new_level,
                } => {
                    handle.model_or_thinking_change(
                        &previous_model,
                        &new_model,
                        &previous_level,
                        &new_level,
                    );
                }
                PendingCmd::ProviderUsage { usage } => {
                    handle.provider_usage(&usage);
                }
                PendingCmd::SetIdentity { identity } => {
                    handle.set_identity(identity);
                }
                PendingCmd::TurnEnd {
                    turn_id,
                    reason,
                    facts,
                } => {
                    handle.turn_end(&turn_id, &reason, facts);
                }
                PendingCmd::BindTurn { host_turn_id } => {
                    handle.bind_turn(&host_turn_id);
                }
            }
        }
    }

    /// Buffer a command if the handle is not yet ready. Returns:
    /// - `Some(handle)` if ready
    /// - `None` if buffered (or terminal / overflow)
    /// - does not block
    fn buffer_or_handle(&self, cmd: PendingCmd) -> Option<CaptureHandle> {
        match self.state() {
            CaptureState::Ready(handle) => return Some(handle),
            // Terminal: nothing will ever replay this buffer.
            CaptureState::Failed(_) | CaptureState::Stopped => return None,
            CaptureState::Opening => {}
        }
        let mut q = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Re-check under lock — the lifecycle may have moved.
        match self.state() {
            CaptureState::Ready(handle) => return Some(handle),
            CaptureState::Failed(_) | CaptureState::Stopped => return None,
            CaptureState::Opening => {}
        }
        if q.len() >= PRE_OPEN_CAP {
            self.pending_overflow.store(true, Ordering::SeqCst);
            self.pending_dropped.fetch_add(1, Ordering::Relaxed);
            warn!(
                cap = PRE_OPEN_CAP,
                "LHC: pre-open buffer full; dropping early item"
            );
            return None;
        }
        q.push_back(cmd);
        None
    }
}

/// Marker type stored in turn-scoped ExtensionData for turn_id correlation.
pub struct LhcTurnId(pub String);

/// Zero-based provider request/response cycle of the active turn, kept in the
/// turn-scoped `ExtensionData` (turn parts, F2 — recorded at intake).
///
/// The host advances it once per outer sampling cycle, before the provider
/// request is sent; transport retries inside that cycle never advance it, so
/// a retried response and its tool results share the cycle of the request
/// they answer. Raw-item capture reads the current value at record time and
/// stamps it on the four step-bearing kinds. Absent (or not yet begun) means
/// unknown: the stored index stays NULL and LHC never splits that turn.
pub struct LhcStepIndex(AtomicI64);

impl LhcStepIndex {
    /// Host seam: begin the next provider cycle for the turn owning
    /// `turn_store`. The first call of a turn yields cycle 0.
    pub fn begin_cycle(turn_store: &ExtensionData) {
        turn_store
            .get_or_init(|| LhcStepIndex(AtomicI64::new(-1)))
            .0
            .fetch_add(1, Ordering::SeqCst);
    }

    /// The cycle in progress for `turn_store`, or `None` before the first
    /// cycle (turn-start input, host context) or outside any turn.
    pub fn current(turn_store: Option<&ExtensionData>) -> Option<i64> {
        turn_store?
            .get::<LhcStepIndex>()
            .map(|index| index.0.load(Ordering::SeqCst))
            .filter(|index| *index >= 0)
    }
}

/// Backward-compat name used by tests.
#[allow(dead_code)]
pub type LhcCaptureHandle = CaptureHandle;

/// Install LHC capture contributors onto the extension registry builder.
///
/// `model_label` / `thinking_level_label` extract wire labels from the host
/// config type (e.g. `Config::model` and `Config::model_reasoning_effort`).
/// `cwd` extracts the session working directory for the LHC thread record.
pub fn install<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    lhc_enabled: impl Fn(&C) -> bool + Send + Sync + 'static,
    model_label: impl Fn(&C) -> String + Send + Sync + 'static,
    thinking_level_label: impl Fn(&C) -> String + Send + Sync + 'static,
    cwd: impl Fn(&C) -> Option<String> + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    // Default provider label: "openai" (Responses). Hosts that need a
    // different provider can use install_with_provider_label.
    install_with_provider_label(
        registry,
        lhc_enabled,
        model_label,
        |_c| "openai".to_string(),
        thinking_level_label,
        cwd,
    );
}

/// Like [`install`], but with an explicit provider id for thinking-signature
/// provenance (R2).
pub fn install_with_provider_label<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    lhc_enabled: impl Fn(&C) -> bool + Send + Sync + 'static,
    model_label: impl Fn(&C) -> String + Send + Sync + 'static,
    provider_label: impl Fn(&C) -> String + Send + Sync + 'static,
    thinking_level_label: impl Fn(&C) -> String + Send + Sync + 'static,
    cwd: impl Fn(&C) -> Option<String> + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(LhcExtension {
        enabled: Arc::new(lhc_enabled),
        model_label: Arc::new(model_label),
        provider_label: Arc::new(provider_label),
        thinking_level_label: Arc::new(thinking_level_label),
        cwd: Arc::new(cwd),
        root_override: None,
        hold_open: false,
        open_thread_fault: None,
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
    registry.raw_item_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    // Retrieval tools (get_turns / get_messages) — same LhcCapture gate as the
    // slot: tools() only contributes when the slot is present.
    registry.tool_contributor(extension);
}

/// Test helper: install with a forced LHC root directory.
#[cfg(any(test, feature = "test-util"))]
pub fn install_with_root<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    lhc_enabled: impl Fn(&C) -> bool + Send + Sync + 'static,
    root: PathBuf,
) where
    C: Send + Sync + 'static,
{
    install_with_root_and_labels(
        registry,
        lhc_enabled,
        root,
        |_c| "unknown".into(),
        |_c| "none".into(),
        |_c| None,
    );
}

/// Test helper: install with a forced root but leave the capture slot in
/// `Opening` — no background open is scheduled (LIM-134).
///
/// Readiness proofs drive the transition themselves via
/// [`LhcCaptureSlot::publish_ready_for_test`] and friends, so "held open" is a
/// barrier the test controls rather than a race against real SQLite.
#[cfg(any(test, feature = "test-util"))]
pub fn install_with_root_held_open<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    lhc_enabled: impl Fn(&C) -> bool + Send + Sync + 'static,
    root: PathBuf,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(LhcExtension {
        enabled: Arc::new(lhc_enabled),
        model_label: Arc::new(|_c| "unknown".into()),
        provider_label: Arc::new(|_c| "openai".to_string()),
        thinking_level_label: Arc::new(|_c| "none".into()),
        cwd: Arc::new(|_c| None),
        root_override: Some(root),
        hold_open: true,
        open_thread_fault: None,
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
    registry.raw_item_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    registry.tool_contributor(extension);
}

/// Test helper: install with a forced root and a fault injected into the
/// dedicated open thread, so the production scheduling path — not a direct
/// `publish_failed` call — is what terminalizes the slot (LIM-134).
#[cfg(any(test, feature = "test-util"))]
pub fn install_with_root_and_open_fault<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    lhc_enabled: impl Fn(&C) -> bool + Send + Sync + 'static,
    root: PathBuf,
    fault: OpenThreadFault,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(LhcExtension {
        enabled: Arc::new(lhc_enabled),
        model_label: Arc::new(|_c| "unknown".into()),
        provider_label: Arc::new(|_c| "openai".to_string()),
        thinking_level_label: Arc::new(|_c| "none".into()),
        cwd: Arc::new(|_c| None),
        root_override: Some(root),
        hold_open: false,
        open_thread_fault: Some(fault),
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
    registry.raw_item_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    registry.tool_contributor(extension);
}

/// Test helper with custom model/thinking extractors.
#[cfg(any(test, feature = "test-util"))]
pub fn install_with_root_and_labels<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    lhc_enabled: impl Fn(&C) -> bool + Send + Sync + 'static,
    root: PathBuf,
    model_label: impl Fn(&C) -> String + Send + Sync + 'static,
    thinking_level_label: impl Fn(&C) -> String + Send + Sync + 'static,
    cwd: impl Fn(&C) -> Option<String> + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(LhcExtension {
        enabled: Arc::new(lhc_enabled),
        model_label: Arc::new(model_label),
        provider_label: Arc::new(|_c| "openai".to_string()),
        thinking_level_label: Arc::new(thinking_level_label),
        cwd: Arc::new(cwd),
        root_override: Some(root),
        hold_open: false,
        open_thread_fault: None,
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
    registry.raw_item_contributor(extension.clone());
    registry.config_contributor(extension.clone());
    registry.tool_contributor(extension);
}

struct LhcExtension<C> {
    enabled: Arc<dyn Fn(&C) -> bool + Send + Sync>,
    model_label: Arc<dyn Fn(&C) -> String + Send + Sync>,
    provider_label: Arc<dyn Fn(&C) -> String + Send + Sync>,
    thinking_level_label: Arc<dyn Fn(&C) -> String + Send + Sync>,
    cwd: Arc<dyn Fn(&C) -> Option<String> + Send + Sync>,
    root_override: Option<PathBuf>,
    /// Test-only (LIM-134): insert the slot but never schedule the open, so a
    /// readiness proof owns the `Opening` → settled transition.
    hold_open: bool,
    /// Test-only (LIM-134): fault injected into the dedicated open thread.
    /// Production is always `None` (the seam type is uninhabited).
    open_thread_fault: Option<OpenThreadFaultSeam>,
}

impl<C: Sync> LhcExtension<C> {
    fn root(&self) -> PathBuf {
        self.root_override.clone().unwrap_or_else(lhc_root)
    }
}

/// Terminalizes the dedicated open thread (LIM-134).
///
/// Every exit of the open closure must leave the slot settled, or a readiness
/// waiter parks forever. Explicit `Ready` / `Failed` disarm this guard; any
/// other exit — an unwind, or a future early `return` that forgets to publish —
/// drops it armed and publishes `Failed`. `publish_failed` only transitions
/// from `Opening`, so a slot already `Stopped` (or `Ready`) is left alone.
struct OpenCompletionGuard {
    slot: Arc<LhcCaptureSlot>,
    thread_id: String,
    armed: bool,
}

impl OpenCompletionGuard {
    fn new(slot: Arc<LhcCaptureSlot>, thread_id: String) -> Self {
        Self {
            slot,
            thread_id,
            armed: true,
        }
    }

    /// Call only after Ready / Failed / Stopped has actually been handled.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OpenCompletionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.slot.publish_failed(CAPTURE_OPEN_ABANDONED) {
            error!(
                thread_id = %self.thread_id,
                telemetry_event = "lhc.capture_open_abandoned",
                "LHC: capture open thread exited without settling the slot"
            );
        }
    }
}

/// Test-only fault injection for the dedicated open thread (LIM-134).
///
/// The injection points are the OS-facing calls only; everything downstream —
/// the spawn-error branch and [`OpenCompletionGuard`] — is production code.
#[cfg(any(test, feature = "test-util"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenThreadFault {
    /// The native thread spawn fails.
    SpawnFailed,
    /// The open thread unwinds before settling the slot.
    PanicBeforeSettle,
}

/// Spawn the dedicated open thread. The single seam a test can fail.
fn spawn_open_thread(
    name: String,
    #[allow(unused_variables)] fault: Option<OpenThreadFaultSeam>,
    body: impl FnOnce() + Send + 'static,
) -> std::io::Result<()> {
    #[cfg(any(test, feature = "test-util"))]
    if fault == Some(OpenThreadFault::SpawnFailed) {
        return Err(std::io::Error::other("injected open-thread spawn failure"));
    }
    std::thread::Builder::new()
        .name(name)
        .spawn(body)
        .map(|_| ())
}

#[cfg(any(test, feature = "test-util"))]
type OpenThreadFaultSeam = OpenThreadFault;
/// Production carries no fault seam; the parameter collapses to a unit value.
#[cfg(not(any(test, feature = "test-util")))]
type OpenThreadFaultSeam = std::convert::Infallible;

/// Schedule a background open; never blocks the caller on SQLite (F17).
///
/// LIM-134: every exit terminalizes the slot. A failed native spawn publishes
/// `Failed` on the caller's thread; anything that leaves the open closure
/// without publishing is caught by [`OpenCompletionGuard`].
fn schedule_open(
    slot: Arc<LhcCaptureSlot>,
    thread_id: String,
    cwd: Option<String>,
    root: PathBuf,
    initial_identity: Option<ModelIdentity>,
    fault: Option<OpenThreadFaultSeam>,
) {
    if slot
        .open_scheduled
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let derivation = slot.derivation_callbacks.clone();
    let slot_for_spawn = Arc::clone(&slot);
    let thread_id_for_spawn = thread_id.clone();
    let spawned = spawn_open_thread(format!("lhc-open-{thread_id}"), fault, move || {
        let mut guard = OpenCompletionGuard::new(Arc::clone(&slot), thread_id.clone());
        #[cfg(any(test, feature = "test-util"))]
        if fault == Some(OpenThreadFault::PanicBeforeSettle) {
            panic!("injected open-thread panic before settlement");
        }
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                error!(?err, "LHC: open runtime failed");
                slot.publish_failed(CAPTURE_OPEN_RUNTIME_UNAVAILABLE);
                guard.disarm();
                return;
            }
        };
        let handle = rt.block_on(spawn_capture_with_identity(
            &thread_id,
            cwd.as_deref(),
            Some(root),
            derivation,
            initial_identity,
        ));
        match handle {
            Some(h) => {
                if slot.set_and_flush(h.clone()) {
                    debug!(thread_id = %thread_id, "LHC: capture opened (async) + pre-open buffer flushed");
                    guard.disarm();
                } else {
                    // LIM-134: the slot went terminal (thread stop) while
                    // this inline SQLite open was still running. Do not
                    // publish and do not replay; close what we opened.
                    warn!(
                        thread_id = %thread_id,
                        "LHC: capture open completed after the slot was stopped; discarding handle"
                    );
                    // The slot is already terminal, so the guard would be a
                    // no-op; disarm to keep "handled" explicit.
                    guard.disarm();
                    rt.block_on(h.shutdown_bounded(CAPTURE_SHUTDOWN_BOUND));
                }
            }
            None => {
                slot.publish_failed(CAPTURE_OPEN_FAILED);
                guard.disarm();
                error!(thread_id = %thread_id, "LHC: failed to open capture");
            }
        }
    });
    if let Err(err) = spawned {
        error!(
            thread_id = %thread_id_for_spawn,
            ?err,
            telemetry_event = "lhc.capture_open_thread_unavailable",
            "LHC: failed to spawn the capture open thread"
        );
        slot_for_spawn.publish_failed(CAPTURE_OPEN_THREAD_UNAVAILABLE);
    }
}

#[cfg(not(test))]
const CAPTURE_SHUTDOWN_BOUND: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
const CAPTURE_SHUTDOWN_BOUND: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(not(test))]
const NATIVE_SHUTDOWN_REPLY_BOUND: std::time::Duration = std::time::Duration::from_secs(12);
#[cfg(test)]
const NATIVE_SHUTDOWN_REPLY_BOUND: std::time::Duration = std::time::Duration::from_secs(3);

async fn shutdown_capture_send(handle: CaptureHandle) -> CaptureShutdownResult {
    let thread_id = handle.thread_id().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawn = std::thread::Builder::new()
        .name("lhc-shutdown".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    error!(?err, "LHC: failed to create capture shutdown runtime");
                    let _ = tx.send(CaptureShutdownResult::Failed(
                        CaptureShutdownFailure::RuntimeUnavailable,
                    ));
                    return;
                }
            };
            let result = rt.block_on(handle.shutdown_bounded(CAPTURE_SHUTDOWN_BOUND));
            let _ = tx.send(result);
        });
    if let Err(err) = spawn {
        error!(
            thread_id = %thread_id,
            ?err,
            durability = "unknown",
            telemetry_event = "lhc.capture_shutdown_durability_unknown",
            "LHC: failed to spawn capture shutdown thread; process exit will continue"
        );
        return CaptureShutdownResult::Failed(CaptureShutdownFailure::RuntimeUnavailable);
    }
    let result = match tokio::time::timeout(NATIVE_SHUTDOWN_REPLY_BOUND, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            error!(
                thread_id = %thread_id,
                failure = "native_reply_dropped",
                durability = "unknown",
                telemetry_event = "lhc.capture_shutdown_durability_unknown",
                "LHC: capture shutdown thread dropped its reply; process exit will continue"
            );
            return CaptureShutdownResult::Failed(CaptureShutdownFailure::AcknowledgementDropped);
        }
        Err(_) => {
            error!(
                thread_id = %thread_id,
                failure = "native_reply_timed_out",
                durability = "unknown",
                telemetry_event = "lhc.capture_shutdown_durability_unknown",
                "LHC: capture shutdown thread timed out; process exit will continue"
            );
            return CaptureShutdownResult::Failed(CaptureShutdownFailure::AcknowledgementTimedOut);
        }
    };
    match result {
        CaptureShutdownResult::Persisted => {
            debug!(thread_id = %thread_id, "LHC: capture shutdown persisted");
        }
        CaptureShutdownResult::Failed(failure @ CaptureShutdownFailure::PersistenceFailed) => {
            error!(
                thread_id = %thread_id,
                failure = failure.as_str(),
                durability = "failed",
                telemetry_event = "lhc.capture_shutdown_persistence_failed",
                "LHC: capture shutdown found a known persistence failure; process exit will continue"
            );
        }
        CaptureShutdownResult::Failed(failure) => {
            error!(
                thread_id = %thread_id,
                failure = failure.as_str(),
                durability = "unknown",
                telemetry_event = "lhc.capture_shutdown_durability_unknown",
                "LHC: capture shutdown failed; process exit will continue"
            );
        }
    }
    result
}

impl<C: Send + Sync + 'static> ThreadLifecycleContributor<C> for LhcExtension<C> {
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if !(self.enabled)(input.config) {
                return;
            }
            // Insert the slot value (ExtensionData wraps it in Arc). Clone via
            // get after insert for the background open.
            input.thread_store.insert(LhcCaptureSlot::new());
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                error!("LHC: slot missing immediately after insert");
                return;
            };
            let thread_id = input.thread_store.level_id().to_string();
            let root = self.root();
            let cwd = (self.cwd)(input.config);
            let model = (self.model_label)(input.config);
            let provider = (self.provider_label)(input.config);
            let identity = ModelIdentity::new(provider, model, ModelIdentity::RESPONSES_API);
            if self.hold_open {
                debug!(thread_id = %thread_id, "LHC: capture slot held Opening (test)");
                return;
            }
            // Fire-and-forget open — Session construction continues immediately.
            schedule_open(
                slot,
                thread_id,
                cwd,
                root,
                Some(identity),
                self.open_thread_fault,
            );
        })
    }

    fn on_thread_resume<'a>(&'a self, input: ThreadResumeInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            // Session construction always runs on_thread_start (including resume).
            let _ = input;
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(slot) = input.thread_store.get::<LhcCaptureSlot>()
                && let Some(handle) = slot.get()
            {
                handle.flush_async();
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() {
                // LIM-134: an open that is still in flight gets the existing
                // shutdown bound to land. If it does, its ordered pre-open
                // replay has already run (Ready publishes only after the
                // final drain pass) and we run the normal bounded shutdown.
                let settled = slot.await_settled(CAPTURE_SHUTDOWN_BOUND).await;
                apply_thread_stop(&slot, settled).await;
            }
        })
    }
}

/// Apply thread-stop to a settled (or timed-out) capture slot.
///
/// `None` is bound expiry: re-read the slot, because a `Ready` that landed
/// inside the timeout window must still take the Ready path (flush already
/// ran at publish; the handle still needs a bounded shutdown). Stomping a
/// live Ready with `stop_and_drop_pending` would skip that shutdown.
async fn apply_thread_stop(slot: &LhcCaptureSlot, settled: Option<CaptureState>) {
    let state = match settled {
        Some(state) => state,
        None => slot.state(),
    };
    match state {
        CaptureState::Ready(handle) => {
            // Refuse retrieval before the worker teardown so tools
            // cannot race a half-shutdown channel/session.
            slot.publish_stopped();
            shutdown_capture_send(handle).await;
        }
        CaptureState::Failed(_) | CaptureState::Stopped => {
            slot.publish_stopped();
        }
        CaptureState::Opening => {
            slot.stop_and_drop_pending();
        }
    }
}

impl<C: Send + Sync + 'static> ToolContributor for LhcExtension<C> {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ToolCall>>> {
        // Same gate as capture: slot is only inserted when LhcCapture is on at
        // thread start. No slot → no retrieval tools.
        let Some(slot) = thread_store.get::<LhcCaptureSlot>() else {
            return Vec::new();
        };
        crate::tools::retrieval_tools(slot)
    }
}

fn turn_end_timing_facts(
    outcome: Option<&'static str>,
    outcome_reason: Option<String>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
) -> TurnEndFacts {
    TurnEndFacts {
        outcome,
        outcome_reason,
        started_at: started_at.map(unix_secs_to_iso),
        ended_at: completed_at.map(unix_secs_to_iso),
    }
}

fn abort_reason_label(reason: &TurnAbortReason) -> String {
    match reason {
        TurnAbortReason::Interrupted => "interrupted".into(),
        TurnAbortReason::Replaced => "replaced".into(),
        TurnAbortReason::ReviewEnded => "review_ended".into(),
        TurnAbortReason::BudgetLimited => "budget_limited".into(),
    }
}

fn dispatch_turn_end(slot: &LhcCaptureSlot, turn_id: &str, reason: &str, facts: TurnEndFacts) {
    let cmd = PendingCmd::TurnEnd {
        turn_id: turn_id.to_string(),
        reason: reason.to_string(),
        facts: facts.clone(),
    };
    if let Some(handle) = slot.buffer_or_handle(cmd) {
        handle.turn_end(turn_id, reason, facts);
    }
}

impl<C: Send + Sync + 'static> TurnLifecycleContributor for LhcExtension<C> {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input
                .turn_store
                .insert(LhcTurnId(input.turn_id.to_string()));
            // Turn parts (AC-7.4 host side): bind the durable turn this host
            // turn's prompt opens to the host identity, so the settled seam can
            // compare `host_metadata.active_turn.turn_id` with it exactly.
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            if let Some(handle) = slot.buffer_or_handle(PendingCmd::BindTurn {
                host_turn_id: input.turn_id.to_string(),
            }) {
                handle.bind_turn(input.turn_id);
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            let turn_id = input
                .turn_store
                .get::<LhcTurnId>()
                .map(|t| t.0.clone())
                .unwrap_or_else(|| "unknown".into());
            let facts = turn_end_timing_facts(
                Some("completed"),
                None,
                input.started_at,
                input.completed_at,
            );
            dispatch_turn_end(&slot, &turn_id, "completed", facts);
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            let turn_id = input
                .turn_store
                .get::<LhcTurnId>()
                .map(|t| t.0.clone())
                .unwrap_or_else(|| "unknown".into());
            let facts = turn_end_timing_facts(
                Some("aborted"),
                Some(abort_reason_label(&input.reason)),
                input.started_at,
                input.completed_at,
            );
            dispatch_turn_end(&slot, &turn_id, "aborted", facts);
            // F-L3: flush before process can exit after SIGINT. Graceful
            // interrupt delivers on_turn_abort then may shut down immediately;
            // without an await here the capture worker may not land the row.
            if let Some(handle) = slot.get() {
                handle.flush().await;
            }
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            // Mid-turn error path: close with aborted + reason when known; no
            // host timing is on TurnErrorInput today (optional fields stay None).
            let facts = turn_end_timing_facts(
                Some("aborted"),
                Some(format!("{:?}", input.error)),
                None,
                None,
            );
            dispatch_turn_end(&slot, input.turn_id, "error", facts);
        })
    }
}

impl<C: Send + Sync + 'static> TokenUsageContributor for LhcExtension<C> {
    fn on_token_usage<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        token_usage: &'a TokenUsageInfo,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(slot) = thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            // last_token_usage is the per-model-call figure from
            // ResponseEvent::Completed (D3).
            let usage = token_usage.last_token_usage.clone();
            let cmd = PendingCmd::ProviderUsage {
                usage: usage.clone(),
            };
            if let Some(handle) = slot.buffer_or_handle(cmd) {
                handle.provider_usage(&usage);
            }
        })
    }
}

impl<C: Send + Sync + 'static> RawItemContributor for LhcExtension<C> {
    fn on_raw_items<'a>(&'a self, input: RawItemInput<'a>) -> ExtensionFuture<'a, ()> {
        // Sync-ready: no LHC await. Panic containment is also at the core hook.
        Box::pin(async move {
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            // Turn parts (F2): the host's provider cycle at record time.
            let step_index = LhcStepIndex::current(input.turn_store);
            // Turn parts (Flow 7): a human prompt recorded while the current
            // host turn has already begun provider cycles is an in-run steer.
            // The fact is the host's own turn lifecycle (`LhcStepIndex` is
            // begun by `run_turn` before each sampling request and lives in
            // the turn-scoped store): the opening prompt is recorded before
            // any cycle begins and is never stamped; a prompt drained from
            // the pending queue inside the loop always is. Never inferred
            // from text.
            let steer =
                matches!(input.provenance, RawItemProvenance::UserPrompt) && step_index.is_some();
            for item in input.items {
                let cmd = PendingCmd::Persist {
                    item: item.clone(),
                    provenance: input.provenance,
                    step_index,
                    steer,
                };
                if let Some(handle) = slot.buffer_or_handle(cmd) {
                    if steer {
                        handle.persist_steer_prompt(item);
                    } else {
                        handle.persist(item, input.provenance, step_index);
                    }
                }
            }
        })
    }
}

impl<C: Send + Sync + 'static> ConfigContributor<C> for LhcExtension<C> {
    /// Cheap: try_send onto the capture queue only. No SQLite, no await.
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
        previous_config: &C,
        new_config: &C,
    ) {
        let previous_model = (self.model_label)(previous_config);
        let new_model = (self.model_label)(new_config);
        let previous_level = (self.thinking_level_label)(previous_config);
        let new_level = (self.thinking_level_label)(new_config);
        let previous_provider = (self.provider_label)(previous_config);
        let new_provider = (self.provider_label)(new_config);
        if previous_model == new_model
            && previous_level == new_level
            && previous_provider == new_provider
        {
            return;
        }
        let Some(slot) = thread_store.get::<LhcCaptureSlot>() else {
            return;
        };
        // Keep R2 signature provenance current: any model/provider change
        // refreshes the capture identity so replay gating compares against
        // what actually produced later thinking. Pre-open changes buffer and
        // replay in order once the handle opens.
        if previous_model != new_model || previous_provider != new_provider {
            let identity = ModelIdentity::new(
                new_provider,
                new_model.clone(),
                ModelIdentity::RESPONSES_API,
            );
            let cmd = PendingCmd::SetIdentity {
                identity: identity.clone(),
            };
            if let Some(handle) = slot.buffer_or_handle(cmd) {
                handle.set_identity(identity);
            }
        }
        // Provider-only changes refresh identity above but are not a
        // model/thinking record event.
        if previous_model == new_model && previous_level == new_level {
            return;
        }
        let cmd = PendingCmd::ModelOrThinkingChange {
            previous_model: previous_model.clone(),
            new_model: new_model.clone(),
            previous_level: previous_level.clone(),
            new_level: new_level.clone(),
        };
        if let Some(handle) = slot.buffer_or_handle(cmd) {
            handle.model_or_thinking_change(
                &previous_model,
                &new_model,
                &previous_level,
                &new_level,
            );
        }
    }
}

/// Wait until the capture handle is ready (tests).
#[cfg(any(test, feature = "test-util"))]
pub async fn wait_for_handle(
    slot: &LhcCaptureSlot,
    timeout: std::time::Duration,
) -> Option<CaptureHandle> {
    match slot.await_settled(timeout).await? {
        CaptureState::Ready(handle) => Some(handle),
        CaptureState::Opening | CaptureState::Failed(_) | CaptureState::Stopped => None,
    }
}

// Silence unused import when only used via trait methods in some cfgs.
#[allow(dead_code)]
fn _provenance_link() -> RawItemProvenance {
    RawItemProvenance::UserPrompt
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::LhcSession;
    use lhc::sdk::DrainOpts;
    use lhc::sdk::OpResult;
    use lhc::shared_tech::CompressDetailedTurnInput;
    use lhc::shared_tech::SmoothPromptInput;
    use lhc::shared_tech::SummarizeChunkBriefInput;
    use lhc::shared_tech::SummarizeToolResultInput;
    use lhc::shared_tech::create_deterministic_inference_callbacks;
    use std::sync::atomic::AtomicUsize;

    fn counting_callbacks(counter: Arc<AtomicUsize>) -> InferenceCallbacks {
        let base = create_deterministic_inference_callbacks();
        macro_rules! wrap {
            ($field:ident, $ty:ty) => {{
                let counter = Arc::clone(&counter);
                let inner = Arc::clone(&base.$field);
                Arc::new(move |input: $ty| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let inner = Arc::clone(&inner);
                    Box::pin(async move { inner(input).await })
                        as lhc::shared_tech::derivation::BoxFuture<
                            lhc::shared_tech::InferenceResult,
                        >
                })
            }};
        }
        InferenceCallbacks {
            smooth_prompt: wrap!(smooth_prompt, SmoothPromptInput),
            summarize_tool_result: wrap!(summarize_tool_result, SummarizeToolResultInput),
            compress_detailed_turn: wrap!(compress_detailed_turn, CompressDetailedTurnInput),
            summarize_chunk_brief: wrap!(summarize_chunk_brief, SummarizeChunkBriefInput),
        }
    }

    fn msg(role: &str, text: &str, id: &str) -> ResponseItem {
        use codex_protocol::models::ContentItem;
        ResponseItem::Message {
            id: Some(codex_protocol::ResponseItemId::from_server(id.into())),
            role: role.into(),
            content: vec![if role == "assistant" {
                ContentItem::OutputText { text: text.into() }
            } else {
                ContentItem::InputText { text: text.into() }
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    /// Build a real registry, drive `on_thread_start` through the production
    /// lifecycle contributors, seed derivation callbacks **before** any capture
    /// (so background derivation runs with them from the first commit), and
    /// capture a bandable thread through the production path.
    async fn seeded_slot_with_callbacks(
        root: &std::path::Path,
        tid: &str,
        callbacks: InferenceCallbacks,
    ) -> Arc<LhcCaptureSlot> {
        let mut builder = ExtensionRegistryBuilder::<()>::new();
        install_with_root(&mut builder, |_c| true, root.to_path_buf());
        let registry = builder.build();
        let store = ExtensionData::new(tid.to_string());
        let session_store = ExtensionData::new("s".to_string());
        let config = ();
        let session_source = codex_protocol::protocol::SessionSource::Exec;
        let environments = [];
        for contributor in registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_start(ThreadStartInput {
                    config: &config,
                    session_source: &session_source,
                    persistent_thread_state_available: false,
                    environments: &environments,
                    mcp_resource_client: None,
                    extension_metrics: None,
                    session_store: &session_store,
                    thread_store: &store,
                })
                .await;
        }
        let slot = store.get::<LhcCaptureSlot>().expect("slot");
        slot.set_derivation_callbacks(callbacks);
        let handle = wait_for_handle(&slot, std::time::Duration::from_secs(30))
            .await
            .expect("handle");
        let pad = "x".repeat(2500);
        for i in 0..80 {
            handle.persist(
                &msg(
                    "user",
                    &format!("user turn {i} bandable {pad}"),
                    &format!("u{i}"),
                ),
                RawItemProvenance::UserPrompt,
                /*step_index*/ None,
            );
            handle.persist(
                &msg(
                    "assistant",
                    &format!("assistant reply {i} bandable {pad}"),
                    &format!("a{i}"),
                ),
                RawItemProvenance::ModelOutput,
                /*step_index*/ None,
            );
        }
        handle.flush().await;
        slot
    }

    /// Background mode: LHC derives on its own as intake commits, with **no**
    /// host drain call anywhere. This is the behaviour the whole M1 idle pump
    /// was hand-rolling, and it is why that pump is gone.
    ///
    /// Driven through the production capture path; the only host action is
    /// seeding production callbacks through the same lifecycle seam the compact
    /// arm uses.
    #[tokio::test]
    async fn background_mode_derives_without_any_host_drain() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let tid = "bg-derives";

        let counter = Arc::new(AtomicUsize::new(0));
        let slot =
            seeded_slot_with_callbacks(root, tid, counting_callbacks(Arc::clone(&counter))).await;

        // Nothing below calls `work.drain`. If derivation happens, the
        // scheduler did it.
        let handle = slot.get().expect("handle");
        let settled = handle
            .drain_settled(std::time::Duration::from_secs(120))
            .await;
        assert!(settled, "background drain did not settle within 120s");

        let derived = counter.load(Ordering::SeqCst);
        assert!(
            derived > 0,
            "background mode must derive without a host drain; got 0 calls. \
             `SdkMode::Manual` leaves the scheduler inert (sdk.rs: poke/touch \
             are no-op closures), which is the misconfiguration this replaces."
        );

        let backlog = drain_backlog(root, tid).await;
        assert_eq!(
            backlog, 0,
            "background mode must leave no claimable work behind: {backlog} \
             items still queued after {derived} derivations"
        );
    }

    /// Remaining claimable derivation work in the archive.
    async fn drain_backlog(root: &std::path::Path, tid: &str) -> i64 {
        let cbs = crate::inference::lhc_inference_callbacks(false).unwrap();
        let (s, _) = LhcSession::open_with_inference(tid, None, Some(root), cbs)
            .await
            .unwrap();
        let r = match s
            .lhc
            .work
            .drain(s.thread_ref.clone(), Some(DrainOpts { max_items: Some(0) }))
            .await
        {
            OpResult::Ok { value } => value.remaining,
            OpResult::Err { error } => panic!("{}", error.reason),
        };
        s.close().await;
        r
    }

    /// L3: the **current** body's provenance is pinned and survives eviction,
    /// driven against the real `SESSION_DERIVED_CAP`.
    #[test]
    fn l3_current_body_provenance_survives_cap_pressure() {
        let slot = LhcCaptureSlot::new();

        // Enough write-backs to push far past the cap: every *current* body must
        // remain fully protected; only superseded history may be trimmed.
        let rounds = (SESSION_DERIVED_CAP / 5) + 20;
        for round in 0..rounds {
            let ids: Vec<String> = (0..5).map(|i| format!("r{round}-id{i}")).collect();
            let digests: Vec<String> = (0..5).map(|i| format!("r{round}-d{i}")).collect();
            slot.mark_derived_after_writeback(ids.clone(), digests.clone())
                .expect("mark");
            let have = slot.derived_ids();
            for id in &ids {
                assert!(
                    have.contains(id),
                    "L3: current body id {id} must not be evicted at round {round} \
                     (cap={SESSION_DERIVED_CAP}); have={} ids",
                    have.len()
                );
            }
            let digs = slot.derived_digests();
            for d in &digests {
                assert!(
                    digs.contains(d),
                    "L3: current body digest {d} must not be evicted at round {round}"
                );
            }
        }
    }

    #[test]
    fn l3_durable_reseed_merges_even_when_cache_warm() {
        let slot = LhcCaptureSlot::new();
        slot.mark_derived_after_writeback(
            vec!["live-a".into(), "live-b".into()],
            vec!["d-a".into(), "d-b".into()],
        )
        .unwrap();
        assert!(!slot.derived_ids().is_empty());
        // Durable record has additional ids not yet in process cache (crash window).
        slot.ensure_derived_from_durable(
            vec!["durable-x".into(), "live-a".into()],
            vec!["d-x".into()],
        );
        let have = slot.derived_ids();
        assert!(have.contains("live-a"));
        assert!(have.contains("live-b"));
        assert!(
            have.contains("durable-x"),
            "L3: durable reseed must merge even when cache non-empty; have={have:?}"
        );
        assert!(slot.derived_digests().contains("d-x"));
    }

    /// L3: superseded provenance — and only superseded — is subject to the cap.
    #[test]
    fn l3_superseded_only_is_capped() {
        let slot = LhcCaptureSlot::new();
        // Supersede far more than the cap, then install a small current body.
        let old: Vec<String> = (0..SESSION_DERIVED_CAP + 50)
            .map(|i| format!("old{i}"))
            .collect();
        let old_digests: Vec<String> = (0..SESSION_DERIVED_CAP + 50)
            .map(|i| format!("od{i}"))
            .collect();
        slot.mark_derived_after_writeback(old.clone(), old_digests)
            .unwrap();
        slot.mark_derived_after_writeback(
            vec!["new1".into(), "new2".into()],
            vec!["nd1".into(), "nd2".into()],
        )
        .unwrap();

        let have = slot.derived_ids();
        assert!(
            have.contains("new1") && have.contains("new2"),
            "current body must be pinned"
        );
        let old_retained = old.iter().filter(|id| have.contains(*id)).count();
        assert!(
            old_retained <= SESSION_DERIVED_CAP,
            "superseded must be capped at {SESSION_DERIVED_CAP}, retained {old_retained}"
        );
        assert!(
            old_retained < old.len(),
            "fixture: the cap must actually have evicted something \
             ({old_retained} of {} retained)",
            old.len()
        );
    }
}

#[cfg(test)]
#[path = "install_pre_open_tests.rs"]
mod install_pre_open_tests;

#[cfg(test)]
#[path = "install_shutdown_tests.rs"]
mod install_shutdown_tests;

#[cfg(test)]
#[path = "install_lifecycle_tests.rs"]
mod install_lifecycle_tests;
