//! Background capture worker + per-thread handle.
//!
//! Bounded queue; capture never blocks the session path and never panics into
//! core. A full queue drops with a loud `error` + counter (F5). Degradation
//! policy: **lossy under burst is not acceptable for a durable record** —
//! `degraded` latches true and subsequent persists are refused until the
//! worker is recreated (loud terminal state, not silent 80% loss).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_extension_api::RawItemProvenance;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde_json::Map;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::error;
use tracing::warn;

use crate::idempotency::OccurrenceTracker;
use crate::idempotency::item_stable_id;
use crate::mapping::MappedEvent;
use crate::mapping::ModelIdentity;
use crate::mapping::TurnEndFacts;
use crate::mapping::attach_provider_usage;
use crate::mapping::attach_steer;
use crate::mapping::attach_step_index;
use crate::mapping::map_item;
use crate::mapping::map_model_or_thinking_change;
use crate::mapping::map_runtime_note;
use crate::mapping::map_turn_end;
use crate::mapping::token_usage_to_provider_usage;
use crate::projections::ensure_legacy_occurrence;
use crate::session::LhcSession;

/// Bound on the capture queue. Must not block the session path.
/// One slot is reserved for the degradation `RuntimeNote` (H6).
pub const CAPTURE_QUEUE_CAP: usize = 1024;
/// Slots available to normal traffic; last slot reserved for truncation note.
const CAPTURE_USER_CAP: usize = CAPTURE_QUEUE_CAP - 1;

/// After the caller's durability deadline expires while the queue is full,
/// give the worker one short, separate chance to receive an unacknowledged
/// shutdown command. This cannot turn an unknown result into success, but it
/// avoids leaving an otherwise healthy worker alive on a long-lived host.
#[cfg(not(test))]
const SHUTDOWN_ENQUEUE_CLEANUP_BOUND: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(test)]
const SHUTDOWN_ENQUEUE_CLEANUP_BOUND: std::time::Duration = std::time::Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureShutdownFailure {
    EnqueueTimedOut,
    WorkerClosed,
    AcknowledgementTimedOut,
    AcknowledgementDropped,
    PersistenceFailed,
    RuntimeUnavailable,
}

impl CaptureShutdownFailure {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::EnqueueTimedOut => "enqueue_timed_out",
            Self::WorkerClosed => "worker_closed",
            Self::AcknowledgementTimedOut => "acknowledgement_timed_out",
            Self::AcknowledgementDropped => "acknowledgement_dropped",
            Self::PersistenceFailed => "persistence_failed",
            Self::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureShutdownResult {
    Persisted,
    Failed(CaptureShutdownFailure),
}

#[derive(Default)]
struct CaptureDurability {
    failed: bool,
}

enum CaptureCmd {
    Persist {
        item: ResponseItem,
        provenance: RawItemProvenance,
        /// Host step index (turn parts, F2): the zero-based provider cycle
        /// the item belongs to, read at record time; `None` = unknown.
        step_index: Option<i64>,
        /// In-run steer (turn parts, Flow 7): a user prompt recorded after the
        /// host turn had begun provider cycles. Stamped `payload.steer=true`
        /// so the SDK keeps it inside the open task turn.
        steer: bool,
    },
    TurnEnd {
        turn_id: String,
        /// Idempotency-key discriminator (`completed`/`aborted`/`error`/`stop`).
        reason: String,
        facts: TurnEndFacts,
    },
    /// Per-model-call provider usage from `ResponseEvent::Completed` via
    /// `TokenUsageContributor` (`last_token_usage`).
    ProviderUsage {
        usage: TokenUsage,
    },
    /// Host turn start (turn parts, AC-7.4 host side): bind the durable turn
    /// the intake opens next to this host turn id. Ordered ahead of that
    /// turn's prompt on the same queue.
    BindTurn {
        host_turn_id: String,
    },
    /// Model and/or thinking-level change (from ConfigContributor).
    ModelOrThinkingChange {
        previous_model: String,
        new_model: String,
        previous_level: String,
        new_level: String,
        /// Provider id for thinking-signature provenance (optional).
        provider: Option<String>,
        /// API wire id (e.g. "responses"); defaults to RESPONSES_API when None.
        api: Option<String>,
    },
    /// Set live model identity used for R2 thinking-signature capture.
    SetIdentity {
        identity: ModelIdentity,
    },
    Flush(oneshot::Sender<()>),
    /// Wait, bounded, for the background scheduler to finish draining this
    /// thread. Replies `true` if it settled within the bound.
    DrainSettled {
        timeout: std::time::Duration,
        ack: oneshot::Sender<bool>,
    },
    #[cfg(any(test, feature = "test-util"))]
    ListEvents(oneshot::Sender<Result<Vec<lhc::intake_stream::EventRecord>, String>>),
    #[cfg(any(test, feature = "test-util"))]
    ListTurns(oneshot::Sender<Result<Vec<lhc::turns::TurnRecord>, String>>),
    #[cfg(any(test, feature = "test-util"))]
    CrashMidPersist {
        /// Crash after successfully submitting this many events of the next
        /// Persist (0 = before any).
        after: usize,
        entered: oneshot::Sender<()>,
    },
    #[cfg(any(test, feature = "test-util"))]
    CaptureDisabled(oneshot::Sender<bool>),
    /// Test-only: park the worker until `release` fires (forces queue fill).
    #[cfg(any(test, feature = "test-util"))]
    Block {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    /// Force-injected runtime_note (used for degraded truncation marker).
    RuntimeNote {
        text: String,
        key_suffix: String,
    },
    Shutdown(Option<oneshot::Sender<CaptureShutdownResult>>),
}

struct CaptureShared {
    thread_id: String,
    /// LHC root used for this thread (for compact bridge re-open).
    root: Option<PathBuf>,
    tx: mpsc::Sender<CaptureCmd>,
    dropped: Arc<AtomicU64>,
    /// Latched when a drop occurs or capture is permanently disabled.
    degraded: Arc<AtomicBool>,
    /// Durable turn bound to the current host turn (published by the worker).
    turn_binding: Arc<std::sync::Mutex<Option<TurnBinding>>>,
}

/// Durable identity of the LHC turn opened under one host turn (turn parts,
/// AC-7.4 host side). The SDK names turns itself (`t{order}`); the host turn
/// id (`LhcTurnId`) is bound to that name when the intake reports the turn
/// opened, so the settled seam can compare the durable active turn with the
/// host's current turn identity exactly rather than by presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnBinding {
    pub host_turn_id: String,
    pub lhc_turn_id: String,
}

/// Worker-side binder: the host turn named by the latest `BindTurn` is bound
/// to the durable turn the intake opens for its opening prompt (or the empty
/// open turn that prompt joins). An in-run steer prompt (`payload.steer`)
/// stays a member of that turn: the SDK opens nothing and the binding is
/// left untouched. A forced-boundary continuation re-binds explicitly
/// (`rebind_turn`); nothing else moves a binding.
struct TurnBinder {
    host_turn_id: Option<String>,
    published: Arc<std::sync::Mutex<Option<TurnBinding>>>,
}

impl TurnBinder {
    /// Bind after one committed batch. A prompt normally opens a durable turn
    /// (`Opened` transition). A prompt that joins an already-open empty turn —
    /// the thread's initial turn, or any open turn without members — reports
    /// no transition; then the open turn from the record is the bound one.
    async fn observe(
        &self,
        session: &mut LhcSession,
        events: &[MappedEvent],
        batch: &lhc::intake_stream::BatchResult,
    ) {
        let Some(host_turn_id) = self.host_turn_id.as_ref() else {
            return;
        };
        let opened = batch
            .turn_transitions
            .iter()
            .rev()
            .find(|transition| {
                matches!(
                    transition.action,
                    lhc::intake_stream::TurnTransitionAction::Opened
                )
            })
            .map(|transition| transition.turn_id.clone());
        let lhc_turn_id = match opened {
            Some(id) => id,
            None => {
                // Only an opening prompt can bind; a steer prompt is a member
                // of the already-bound turn and must not move the binding.
                let prompted = events.iter().any(|event| {
                    event.input.event_kind == "user_prompt"
                        && event.input.payload.get("steer") != Some(&Value::Bool(true))
                });
                if !prompted || self.bound_for(host_turn_id) {
                    return;
                }
                match session.list_turns().await {
                    Ok(turns) => match turns
                        .into_iter()
                        .find(|turn| matches!(turn.status, lhc::turns::TurnStatus::Open))
                    {
                        Some(turn) => turn.turn_id,
                        None => return,
                    },
                    Err(err) => {
                        warn!(%err, "LHC: could not resolve the open turn for host turn binding");
                        return;
                    }
                }
            }
        };
        *self
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(TurnBinding {
            host_turn_id: host_turn_id.clone(),
            lhc_turn_id,
        });
    }

    fn bound_for(&self, host_turn_id: &str) -> bool {
        self.published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|binding| binding.host_turn_id == host_turn_id)
    }
}

/// Handle to a per-thread capture worker (cheaply cloneable).
#[derive(Clone)]
pub struct CaptureHandle {
    inner: Arc<CaptureShared>,
}

impl CaptureHandle {
    /// Thread id this capture worker is bound to.
    pub fn thread_id(&self) -> &str {
        &self.inner.thread_id
    }

    /// LHC root directory used when this worker was spawned.
    pub fn root(&self) -> Option<&std::path::Path> {
        self.inner.root.as_deref()
    }

    /// Remaining channel capacity. Reserves one slot for the truncation note.
    fn user_slots_available(&self) -> bool {
        // tokio mpsc::Sender::capacity() = remaining free slots.
        self.inner.tx.capacity() > 1
    }

    /// Non-blocking persist. Drops with error + degrades when the queue is full.
    ///
    /// `step_index` is the host's zero-based provider request/response cycle
    /// for step-bearing kinds (turn parts, F2); it is recorded verbatim and
    /// never inferred. `None` leaves the stored index NULL.
    pub fn persist(
        &self,
        item: &ResponseItem,
        provenance: RawItemProvenance,
        step_index: Option<i64>,
    ) {
        if self.inner.degraded.load(Ordering::Relaxed) {
            self.note_drop("degraded_refuse");
            return;
        }
        if !self.user_slots_available() {
            self.latch_degraded("persist_full");
            return;
        }
        self.send_persist(item, provenance, step_index, /*steer*/ false);
    }

    /// Non-blocking persist of a human prompt the host recorded **after** the
    /// current host turn had begun provider cycles (turn parts, Flow 7). The
    /// prompt is stamped `payload.steer=true` so the SDK keeps it a member of
    /// the open task turn instead of closing it and opening a successor. The
    /// host asserts this from its own turn lifecycle only — never from text.
    pub fn persist_steer_prompt(&self, item: &ResponseItem) {
        self.send_persist(
            item,
            RawItemProvenance::UserPrompt,
            /*step_index*/ None,
            /*steer*/ true,
        );
    }

    fn send_persist(
        &self,
        item: &ResponseItem,
        provenance: RawItemProvenance,
        step_index: Option<i64>,
        steer: bool,
    ) {
        if self.inner.degraded.load(Ordering::Relaxed) {
            self.note_drop("degraded_refuse");
            return;
        }
        if !self.user_slots_available() {
            self.latch_degraded("persist_full");
            return;
        }
        match self.inner.tx.try_send(CaptureCmd::Persist {
            item: item.clone(),
            provenance,
            step_index,
            steer,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.latch_degraded("persist_full");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.latch_degraded("persist_closed");
            }
        }
    }

    /// Bind the durable turn the intake opens next to `host_turn_id` (turn
    /// parts, AC-7.4 host side). Sent at host turn start, so it is ordered
    /// ahead of that turn's prompt on the capture queue. A full or closed
    /// queue degrades like any other lost command: the binding never lands
    /// and the seam keeps the current body.
    pub fn bind_turn(&self, host_turn_id: &str) {
        if self.inner.degraded.load(Ordering::Relaxed) {
            self.note_drop("degraded_refuse");
            return;
        }
        match self.inner.tx.try_send(CaptureCmd::BindTurn {
            host_turn_id: host_turn_id.to_string(),
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.latch_degraded("bind_turn_full");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.latch_degraded("bind_turn_closed");
            }
        }
    }

    /// Re-bind `host_turn_id` to a durable turn the host observed the SDK open
    /// on its behalf (the forced-boundary continuation turn, AC-7.3). Keeps
    /// the exact identity check truthful for a thread that keeps using the
    /// legacy runtime, whose continuation turns no host prompt opens.
    pub fn rebind_turn(&self, host_turn_id: &str, lhc_turn_id: &str) {
        *self
            .inner
            .turn_binding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(TurnBinding {
            host_turn_id: host_turn_id.to_string(),
            lhc_turn_id: lhc_turn_id.to_string(),
        });
    }

    /// The durable LHC turn id bound to `host_turn_id`, once capture has
    /// committed that turn's opening prompt. `None` before then, or when the
    /// current binding belongs to a different host turn.
    pub fn durable_turn_id(&self, host_turn_id: &str) -> Option<String> {
        self.inner
            .turn_binding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|binding| binding.host_turn_id == host_turn_id)
            .map(|binding| binding.lhc_turn_id.clone())
    }

    /// Latch degraded and try to record a self-describing truncation note (H6).
    pub(crate) fn latch_degraded(&self, kind: &str) {
        let n = self.note_drop(kind);
        if self.inner.degraded.swap(true, Ordering::SeqCst) {
            return; // already latched
        }
        error!(
            thread_id = %self.inner.thread_id,
            dropped = n,
            queue_cap = CAPTURE_USER_CAP,
            kind,
            "LHC: capture degraded — refusing further captures; recording truncation note if possible"
        );
        // Reserved slot (capacity > 0 when user traffic was refused at remaining==1)
        // so this note usually lands before further work is refused.
        let text =
            format!("LHC capture degraded ({kind}); subsequent events dropped (dropped_count≈{n})");
        let _ = self.inner.tx.try_send(CaptureCmd::RuntimeNote {
            text,
            key_suffix: format!("degraded-{kind}"),
        });
    }

    pub fn turn_end(&self, turn_id: &str, reason: &str, facts: TurnEndFacts) {
        if self.inner.degraded.load(Ordering::Relaxed) {
            return;
        }
        if !self.user_slots_available() {
            self.latch_degraded("turn_end");
            return;
        }
        match self.inner.tx.try_send(CaptureCmd::TurnEnd {
            turn_id: turn_id.to_string(),
            reason: reason.to_string(),
            facts,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.latch_degraded("turn_end");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.latch_degraded("turn_end_closed");
            }
        }
    }

    /// Attach per-call provider usage to the pending model-output assistant_text
    /// events (schema v5 / D3). No-op when nothing is buffered.
    pub fn provider_usage(&self, usage: &TokenUsage) {
        if self.inner.degraded.load(Ordering::Relaxed) {
            return;
        }
        if !self.user_slots_available() {
            self.latch_degraded("provider_usage");
            return;
        }
        match self.inner.tx.try_send(CaptureCmd::ProviderUsage {
            usage: usage.clone(),
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.latch_degraded("provider_usage");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.latch_degraded("provider_usage_closed");
            }
        }
    }

    /// Set live model identity for R2 thinking-signature capture.
    pub fn set_identity(&self, identity: ModelIdentity) {
        if self.inner.degraded.load(Ordering::Relaxed) {
            return;
        }
        match self.inner.tx.try_send(CaptureCmd::SetIdentity { identity }) {
            Ok(()) => {}
            // Stale provenance is an integrity fault — same discipline as
            // persist: a lost identity update degrades the capture.
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.latch_degraded("identity_full");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.latch_degraded("identity_closed");
            }
        }
    }

    /// Non-blocking model/thinking change. Suppresses no-ops at the call site;
    /// still safe if both sides match (mapper emits zero events).
    pub fn model_or_thinking_change(
        &self,
        previous_model: &str,
        new_model: &str,
        previous_level: &str,
        new_level: &str,
    ) {
        if self.inner.degraded.load(Ordering::Relaxed) {
            return;
        }
        if previous_model == new_model && previous_level == new_level {
            return;
        }
        if !self.user_slots_available() {
            self.latch_degraded("model_change");
            return;
        }
        match self.inner.tx.try_send(CaptureCmd::ModelOrThinkingChange {
            previous_model: previous_model.to_string(),
            new_model: new_model.to_string(),
            previous_level: previous_level.to_string(),
            new_level: new_level.to_string(),
            provider: None,
            api: None,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.latch_degraded("model_change");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.latch_degraded("model_change_closed");
            }
        }
    }

    /// Test-only: park the worker so the queue can be overfilled deterministically.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn block_worker(&self) -> oneshot::Sender<()> {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let _ = self
            .inner
            .tx
            .send(CaptureCmd::Block {
                entered: entered_tx,
                release: release_rx,
            })
            .await;
        let _ = entered_rx.await;
        release_tx
    }

    fn note_drop(&self, kind: &str) -> u64 {
        let n = self.inner.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(
            thread_id = %self.inner.thread_id,
            dropped = n,
            queue_cap = CAPTURE_QUEUE_CAP,
            kind,
            "LHC: capture queue drop"
        );
        n
    }

    pub fn flush_async(&self) {
        let (tx, _rx) = oneshot::channel();
        let _ = self.inner.tx.try_send(CaptureCmd::Flush(tx));
    }

    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.inner.tx.send(CaptureCmd::Flush(tx)).await.is_err() {
            return;
        }
        let _ = rx.await;
    }

    /// Best-effort flush with a hard deadline.
    ///
    /// Compact must not wait unbounded for a busy or wedged capture worker.
    /// A timeout here is fail-open: the caller continues to produce/import/
    /// coverage. It is not a licence to start native compact, and it does not
    /// detach a writer — the worker still owns any in-flight persist.
    ///
    /// Returns `true` only when the worker acknowledged the flush. Enqueue
    /// timeout (full queue / stuck worker), a closed worker, or a dropped /
    /// late ack all return `false`.
    pub async fn flush_within(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let (tx, rx) = oneshot::channel();
        match tokio::time::timeout_at(deadline, self.inner.tx.send(CaptureCmd::Flush(tx))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return false,
        }
        matches!(tokio::time::timeout_at(deadline, rx).await, Ok(Ok(())))
    }

    /// Wait, bounded, for background derivation to settle on this thread.
    ///
    /// Returns `false` if it did not settle in time, which the caller treats as
    /// a fail-open — never as licence to start draining inline. This is the
    /// short settle-wait the design always called for; it replaces the
    /// compact-time drain loop that existed only because the SDK was
    /// misconfigured to `Manual` and its scheduler was inert.
    ///
    /// The bound is enforced **on the worker**, not just here: a settle that
    /// cannot finish (e.g. derivation waiting on callbacks that are never
    /// seeded) must not wedge the worker loop, or every command queued behind
    /// it — including `Shutdown` — would never run. The outer timeout below
    /// only guards a wedged or dead worker; the grace covers queue latency in
    /// front of the command.
    pub async fn drain_settled(&self, timeout: std::time::Duration) -> bool {
        const REPLY_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
        let (tx, rx) = oneshot::channel();
        if self
            .inner
            .tx
            .send(CaptureCmd::DrainSettled { timeout, ack: tx })
            .await
            .is_err()
        {
            return false;
        }
        match tokio::time::timeout(timeout.saturating_add(REPLY_GRACE), rx).await {
            Ok(Ok(settled)) => settled,
            _ => false,
        }
    }

    pub fn shutdown_async(&self) {
        let _ = self.inner.tx.try_send(CaptureCmd::Shutdown(None));
    }

    pub async fn shutdown(self) {
        let (tx, rx) = oneshot::channel();
        if self
            .inner
            .tx
            .send(CaptureCmd::Shutdown(Some(tx)))
            .await
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    pub(crate) async fn shutdown_bounded(
        self,
        timeout: std::time::Duration,
    ) -> CaptureShutdownResult {
        let deadline = tokio::time::Instant::now() + timeout;
        let (tx, rx) = oneshot::channel();
        match tokio::time::timeout_at(deadline, self.inner.tx.send(CaptureCmd::Shutdown(Some(tx))))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return CaptureShutdownResult::Failed(CaptureShutdownFailure::WorkerClosed);
            }
            Err(_) => {
                let cleanup = CaptureCmd::Shutdown(/*ack*/ None);
                let _ = tokio::time::timeout(
                    SHUTDOWN_ENQUEUE_CLEANUP_BOUND,
                    self.inner.tx.send(cleanup),
                )
                .await;
                return CaptureShutdownResult::Failed(CaptureShutdownFailure::EnqueueTimedOut);
            }
        }
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                CaptureShutdownResult::Failed(CaptureShutdownFailure::AcknowledgementDropped)
            }
            Err(_) => {
                CaptureShutdownResult::Failed(CaptureShutdownFailure::AcknowledgementTimedOut)
            }
        }
    }

    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    pub fn is_degraded(&self) -> bool {
        self.inner.degraded.load(Ordering::Relaxed)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub async fn list_events(&self) -> Result<Vec<lhc::intake_stream::EventRecord>, String> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .send(CaptureCmd::ListEvents(tx))
            .await
            .map_err(|_| "capture worker gone".to_string())?;
        rx.await.map_err(|_| "capture worker dropped".to_string())?
    }

    #[cfg(any(test, feature = "test-util"))]
    pub async fn list_turns(&self) -> Result<Vec<lhc::turns::TurnRecord>, String> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .send(CaptureCmd::ListTurns(tx))
            .await
            .map_err(|_| "capture worker gone".to_string())?;
        rx.await.map_err(|_| "capture worker dropped".to_string())?
    }

    /// Crash the worker after successfully submitting `after` events of the
    /// next Persist batch (0 = crash before any submit).
    #[cfg(any(test, feature = "test-util"))]
    pub async fn arm_crash_mid_persist(&self, after: usize) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let _ = self
            .inner
            .tx
            .send(CaptureCmd::CrashMidPersist {
                after,
                entered: entered_tx,
            })
            .await;
        let _ = entered_rx.await;
    }

    #[cfg(any(test, feature = "test-util"))]
    pub async fn is_capture_disabled(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .inner
            .tx
            .send(CaptureCmd::CaptureDisabled(tx))
            .await
            .is_err()
        {
            return true;
        }
        rx.await.unwrap_or(true)
    }
}

/// Spawn a capture worker for `thread_id` under `root`.
pub async fn spawn_capture(
    thread_id: &str,
    cwd: Option<&str>,
    root: Option<PathBuf>,
    derivation: crate::inference::LateBoundCallbacks,
) -> Option<CaptureHandle> {
    spawn_capture_with_identity(thread_id, cwd, root, derivation, None).await
}

/// Spawn capture with an initial model identity for R2 signature provenance.
pub async fn spawn_capture_with_identity(
    thread_id: &str,
    cwd: Option<&str>,
    root: Option<PathBuf>,
    derivation: crate::inference::LateBoundCallbacks,
    initial_identity: Option<ModelIdentity>,
) -> Option<CaptureHandle> {
    // Background mode derives on this session, so its callbacks are what lands
    // in the durable record — never the deterministic ones (J1).
    let (session, tracker) =
        LhcSession::open(thread_id, cwd, root.as_deref(), derivation.callbacks()).await?;
    let (tx, rx) = mpsc::channel(CAPTURE_QUEUE_CAP);
    let dropped = Arc::new(AtomicU64::new(0));
    let degraded = Arc::new(AtomicBool::new(false));
    let turn_binding = Arc::new(std::sync::Mutex::new(None));
    let shared = Arc::new(CaptureShared {
        thread_id: thread_id.to_string(),
        root: root.clone(),
        tx,
        dropped: Arc::clone(&dropped),
        degraded: Arc::clone(&degraded),
        turn_binding: Arc::clone(&turn_binding),
    });
    let thread_id_owned = thread_id.to_string();
    let degraded_worker = Arc::clone(&degraded);
    std::thread::Builder::new()
        .name(format!("lhc-capture-{thread_id}"))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    error!(?err, "LHC: failed to build capture runtime");
                    degraded_worker.store(true, Ordering::SeqCst);
                    return;
                }
            };
            // Contain worker panics so a single bad item cannot darken the
            // thread forever without a degradation note (H9).
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rt.block_on(async move {
                    worker_loop(
                        session,
                        tracker,
                        rx,
                        thread_id_owned,
                        degraded_worker,
                        derivation,
                        initial_identity.unwrap_or_default(),
                        turn_binding,
                    )
                    .await;
                });
            }));
            if let Err(payload) = result {
                let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "non-string panic".into()
                };
                error!(%msg, "LHC: capture worker panicked");
            }
        })
        .map_err(|err| {
            error!(?err, "LHC: failed to spawn capture thread");
            err
        })
        .ok()?;
    Some(CaptureHandle { inner: shared })
}

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    mut session: LhcSession,
    mut tracker: OccurrenceTracker,
    mut rx: mpsc::Receiver<CaptureCmd>,
    thread_id: String,
    degraded: Arc<AtomicBool>,
    derivation: crate::inference::LateBoundCallbacks,
    mut live_identity: ModelIdentity,
    turn_binding: Arc<std::sync::Mutex<Option<TurnBinding>>>,
) {
    #[cfg(any(test, feature = "test-util"))]
    let mut crash_after: Option<usize> = None;
    let mut binder = TurnBinder {
        host_turn_id: None,
        published: turn_binding,
    };
    // ModelOutput items from one sampling call arrive before
    // ResponseEvent::Completed (token usage). Buffer them so assistant_text
    // can carry providerUsage on the same event (schema v5 / D3) without
    // reordering relative to thinking/tool_call siblings.
    let mut pending_model_output: Vec<(ResponseItem, RawItemProvenance, Option<i64>)> = Vec::new();
    let mut durability = CaptureDurability::default();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            CaptureCmd::BindTurn { host_turn_id } => {
                binder.host_turn_id = Some(host_turn_id);
            }
            CaptureCmd::Persist {
                item,
                provenance,
                step_index,
                steer,
            } => {
                if matches!(provenance, RawItemProvenance::ModelOutput) {
                    pending_model_output.push((item, provenance, step_index));
                    continue;
                }
                if let Err(err) = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    None,
                    &live_identity,
                    &binder,
                    &mut durability,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                    && err == "crash"
                {
                    return;
                }
                if let Err(err) = persist_item(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &item,
                    provenance,
                    None,
                    step_index,
                    /*steer*/ steer,
                    &live_identity,
                    &binder,
                    &mut durability,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                    && err == "crash"
                {
                    return;
                }
            }
            CaptureCmd::ProviderUsage { usage } => {
                let provider_usage = token_usage_to_provider_usage(&usage);
                if let Err(err) = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    provider_usage.as_ref(),
                    &live_identity,
                    &binder,
                    &mut durability,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                    && err == "crash"
                {
                    return;
                }
            }
            CaptureCmd::TurnEnd {
                turn_id,
                reason,
                facts,
            } => {
                if let Err(err) = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    None,
                    &live_identity,
                    &binder,
                    &mut durability,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                    && err == "crash"
                {
                    return;
                }
                let event = map_turn_end(&thread_id, &turn_id, &reason, &facts);
                if let Err(err) = submit_mapped(&mut session, &[event]).await {
                    durability.failed = true;
                    warn!(thread_id = %thread_id, %err, "LHC: turn_end failed");
                }
            }
            CaptureCmd::ModelOrThinkingChange {
                previous_model,
                new_model,
                previous_level,
                new_level,
                provider,
                api,
            } => {
                if let Err(err) = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    None,
                    &live_identity,
                    &binder,
                    &mut durability,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                    && err == "crash"
                {
                    return;
                }
                // Keep signature provenance in lockstep with the live model.
                if previous_model != new_model {
                    live_identity.model = Some(new_model.clone());
                    if let Some(p) = provider {
                        live_identity.provider = Some(p);
                    }
                    live_identity.api =
                        Some(api.unwrap_or_else(|| ModelIdentity::RESPONSES_API.to_string()));
                }
                let events = map_model_or_thinking_change(
                    &thread_id,
                    &previous_model,
                    &new_model,
                    &previous_level,
                    &new_level,
                );
                if events.is_empty() {
                    continue;
                }
                if let Err(err) = submit_mapped(&mut session, &events).await {
                    durability.failed = true;
                    warn!(thread_id = %thread_id, %err, "LHC: model/thinking change failed");
                }
            }
            CaptureCmd::SetIdentity { identity } => {
                // Flush model output buffered under the OLD identity first —
                // otherwise pending old-model ciphertext would be mapped and
                // tagged with the new identity (validator P1, 2026-08-08).
                if let Err(err) = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    None,
                    &live_identity,
                    &binder,
                    &mut durability,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                {
                    warn!(thread_id = %thread_id, %err, "LHC: pre-identity-change flush failed");
                }
                live_identity = identity;
            }
            CaptureCmd::RuntimeNote { text, key_suffix } => {
                let event = map_runtime_note(&thread_id, &text, &key_suffix);
                if let Err(err) = submit_mapped(&mut session, &[event]).await {
                    durability.failed = true;
                    warn!(thread_id = %thread_id, %err, "LHC: degraded note failed");
                }
            }
            CaptureCmd::Flush(ack) => {
                // Flush does not force pending model-output without usage —
                // caller that needs durable state should use turn_end or
                // provider_usage first. Still surface buffered content so
                // tests that only flush after provider_usage see it; when
                // nothing has attached usage yet, emit without providerUsage.
                if let Err(err) = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    None,
                    &live_identity,
                    &binder,
                    &mut durability,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                    && err == "crash"
                {
                    return;
                }
                let _ = ack.send(());
            }
            CaptureCmd::DrainSettled { timeout, ack } => {
                // Runs on the worker, which owns the Background-mode SDK — the
                // only session with a live scheduler. Bounded HERE, not only at
                // the handle: this await runs inside the worker loop, so an
                // unbounded settle-wait on a thread that can never settle would
                // wedge the loop and starve every command behind it, including
                // `Shutdown`. Timing out reports unsettled and the caller fails
                // open; it is never licence to drain inline.
                let settled = tokio::time::timeout(timeout, session.drain_settled())
                    .await
                    .is_ok();
                let _ = ack.send(settled);
            }
            #[cfg(any(test, feature = "test-util"))]
            CaptureCmd::ListEvents(ack) => {
                let _ = ack.send(session.list_events().await);
            }
            #[cfg(any(test, feature = "test-util"))]
            CaptureCmd::ListTurns(ack) => {
                let _ = ack.send(session.list_turns().await);
            }
            #[cfg(any(test, feature = "test-util"))]
            CaptureCmd::CrashMidPersist { after, entered } => {
                crash_after = Some(after);
                let _ = entered.send(());
            }
            #[cfg(any(test, feature = "test-util"))]
            CaptureCmd::Block { entered, release } => {
                let _ = entered.send(());
                let _ = release.await;
            }
            #[cfg(any(test, feature = "test-util"))]
            CaptureCmd::CaptureDisabled(ack) => {
                let _ = ack.send(session.capture_disabled);
            }
            CaptureCmd::Shutdown(ack) => {
                let _ = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    None,
                    &live_identity,
                    &binder,
                    &mut durability,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await;
                // Everything ahead of Shutdown has now crossed the SDK submit
                // boundary, including the final pending model output. Report
                // that intake durability before best-effort derivation settle:
                // close may consume its full five-second allowance, but its
                // durable work queue is replayable and is not part of this
                // acknowledgment contract.
                if let Some(ack) = ack {
                    let result = if durability.failed || degraded.load(Ordering::SeqCst) {
                        CaptureShutdownResult::Failed(CaptureShutdownFailure::PersistenceFailed)
                    } else {
                        CaptureShutdownResult::Persisted
                    };
                    let _ = ack.send(result);
                }
                close_capture_session(session, &derivation).await;
                return;
            }
        }
    }
    let _ = flush_pending_model_output(
        &mut session,
        &mut tracker,
        &thread_id,
        &degraded,
        &mut pending_model_output,
        None,
        &live_identity,
        &binder,
        &mut durability,
        #[cfg(any(test, feature = "test-util"))]
        &mut crash_after,
    )
    .await;
    close_capture_session(session, &derivation).await;
}

#[allow(clippy::too_many_arguments)]
async fn flush_pending_model_output(
    session: &mut LhcSession,
    tracker: &mut OccurrenceTracker,
    thread_id: &str,
    degraded: &AtomicBool,
    pending: &mut Vec<(ResponseItem, RawItemProvenance, Option<i64>)>,
    provider_usage: Option<&Map<String, Value>>,
    identity: &ModelIdentity,
    binder: &TurnBinder,
    durability: &mut CaptureDurability,
    #[cfg(any(test, feature = "test-util"))] crash_after: &mut Option<usize>,
) -> Result<(), String> {
    if pending.is_empty() {
        return Ok(());
    }
    let items = std::mem::take(pending);
    for (item, provenance, step_index) in items {
        persist_item(
            session,
            tracker,
            thread_id,
            degraded,
            &item,
            provenance,
            provider_usage,
            step_index,
            /*steer*/ false,
            identity,
            binder,
            durability,
            #[cfg(any(test, feature = "test-util"))]
            crash_after,
        )
        .await?;
    }
    Ok(())
}

async fn persist_item(
    session: &mut LhcSession,
    tracker: &mut OccurrenceTracker,
    thread_id: &str,
    degraded: &AtomicBool,
    item: &ResponseItem,
    provenance: RawItemProvenance,
    provider_usage: Option<&Map<String, Value>>,
    step_index: Option<i64>,
    steer: bool,
    identity: &ModelIdentity,
    binder: &TurnBinder,
    durability: &mut CaptureDurability,
    #[cfg(any(test, feature = "test-util"))] crash_after: &mut Option<usize>,
) -> Result<(), String> {
    // Legacy ID-less items resolve occurrence from a capped prefix listing
    // only when they have no stable id. Cap exhaustion degrades visibly and
    // does not guess or reset tracker state.
    if item_stable_id(item).is_none()
        && let Err(err) = ensure_legacy_occurrence(session, tracker, item).await
    {
        warn!(
            thread_id = %thread_id,
            %err,
            "LHC: refusing anonymous persist after occurrence listing failure"
        );
        durability.failed = true;
        degraded.store(true, Ordering::SeqCst);
        let note = map_runtime_note(
            thread_id,
            &format!("LHC capture degraded after anonymous occurrence listing failure: {err}"),
            "occurrence-cap",
        );
        let _ = submit_mapped(session, &[note]).await;
        return Ok(());
    }

    // Contain map_item panics so one bad item cannot kill the worker (H9).
    let mut local = tracker.clone();
    let id_ref = if identity.is_complete() {
        Some(identity)
    } else {
        None
    };
    let mapped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        map_item(thread_id, item, provenance, &mut local, id_ref)
    }));
    let mut events = match mapped {
        Ok(events) => {
            *tracker = local;
            events
        }
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "non-string panic".into()
            };
            error!(
                thread_id = %thread_id,
                %msg,
                "LHC: map_item panicked; dropping item, worker continues"
            );
            durability.failed = true;
            return Ok(());
        }
    };
    if let Some(usage) = provider_usage {
        for event in &mut events {
            attach_provider_usage(event, usage);
        }
    }
    if let Some(step) = step_index {
        for event in &mut events {
            attach_step_index(event, step);
        }
    }
    if steer {
        for event in &mut events {
            attach_steer(event);
        }
    }
    if events.is_empty() {
        return Ok(());
    }
    #[cfg(any(test, feature = "test-util"))]
    if let Some(after) = crash_after.take() {
        if after == 0 {
            error!(thread_id = %thread_id, "LHC: crash injection before persist");
            return Err("crash".into());
        }
        let n = after.min(events.len());
        let (head, _tail) = events.split_at(n);
        if !head.is_empty() {
            let inputs: Vec<_> = head.iter().map(|e| e.input.clone()).collect();
            let _ = session.submit_events(&inputs).await;
        }
        // after >= events.len() means "after all submitted" — still exit.
        error!(
            thread_id = %thread_id,
            after,
            "LHC: crash injection mid-persist"
        );
        return Err("crash".into());
    }
    match submit_mapped(session, &events).await {
        Ok(batch) => binder.observe(session, &events, &batch).await,
        Err(err) => {
            durability.failed = true;
            warn!(thread_id = %thread_id, %err, "LHC: persist failed");
            if session.capture_disabled {
                degraded.store(true, Ordering::SeqCst);
                error!(
                    thread_id = %thread_id,
                    "LHC: capture permanently disabled after repeated failures"
                );
                // Record a truncation note while still possible.
                let note = map_runtime_note(
                    thread_id,
                    "LHC capture permanently disabled after repeated submit failures",
                    "disabled",
                );
                let _ = submit_mapped(session, &[note]).await;
            }
        }
    }
    Ok(())
}

/// Close the worker's Background-mode session.
///
/// The settle-wait inside [`LhcSession::close`] is skipped when derivation
/// callbacks were never seeded: unseeded inference work is parked on
/// `LateBoundCallbacks::resolve` and can never complete, so waiting on it is
/// pure delay, not cleanup. Either way nothing is lost — the work queue is
/// durable and first-touch catch-up re-drains it on the next open.
async fn close_capture_session(
    session: LhcSession,
    derivation: &crate::inference::LateBoundCallbacks,
) {
    if derivation.is_seeded() {
        session.close().await;
    }
    // else: drop. The claim lease releases the in-flight item.
}

async fn submit_mapped(
    session: &mut LhcSession,
    events: &[MappedEvent],
) -> Result<lhc::intake_stream::BatchResult, String> {
    let inputs: Vec<_> = events.iter().map(|e| e.input.clone()).collect();
    session.submit_events(&inputs).await
}
