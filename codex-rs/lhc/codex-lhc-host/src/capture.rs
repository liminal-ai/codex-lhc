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
use crate::mapping::MappedEvent;
use crate::mapping::TurnEndFacts;
use crate::mapping::attach_provider_usage;
use crate::mapping::map_item;
use crate::mapping::map_model_or_thinking_change;
use crate::mapping::map_runtime_note;
use crate::mapping::map_turn_end;
use crate::mapping::token_usage_to_provider_usage;
use crate::session::LhcSession;

/// Bound on the capture queue. Must not block the session path.
/// One slot is reserved for the degradation `RuntimeNote` (H6).
pub const CAPTURE_QUEUE_CAP: usize = 1024;
/// Slots available to normal traffic; last slot reserved for truncation note.
const CAPTURE_USER_CAP: usize = CAPTURE_QUEUE_CAP - 1;

enum CaptureCmd {
    Persist {
        item: ResponseItem,
        provenance: RawItemProvenance,
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
    /// Model and/or thinking-level change (from ConfigContributor).
    ModelOrThinkingChange {
        previous_model: String,
        new_model: String,
        previous_level: String,
        new_level: String,
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
    Shutdown(Option<oneshot::Sender<()>>),
}

struct CaptureShared {
    thread_id: String,
    /// LHC root used for this thread (for compact bridge re-open).
    root: Option<PathBuf>,
    tx: mpsc::Sender<CaptureCmd>,
    dropped: Arc<AtomicU64>,
    /// Latched when a drop occurs or capture is permanently disabled.
    degraded: Arc<AtomicBool>,
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
    pub fn persist(&self, item: &ResponseItem, provenance: RawItemProvenance) {
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

    /// Latch degraded and try to record a self-describing truncation note (H6).
    fn latch_degraded(&self, kind: &str) {
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
    // Background mode derives on this session, so its callbacks are what lands
    // in the durable record — never the deterministic ones (J1).
    let (session, tracker) =
        LhcSession::open(thread_id, cwd, root.as_deref(), derivation.callbacks()).await?;
    let (tx, rx) = mpsc::channel(CAPTURE_QUEUE_CAP);
    let dropped = Arc::new(AtomicU64::new(0));
    let degraded = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(CaptureShared {
        thread_id: thread_id.to_string(),
        root: root.clone(),
        tx,
        dropped: Arc::clone(&dropped),
        degraded: Arc::clone(&degraded),
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

async fn worker_loop(
    mut session: LhcSession,
    mut tracker: OccurrenceTracker,
    mut rx: mpsc::Receiver<CaptureCmd>,
    thread_id: String,
    degraded: Arc<AtomicBool>,
    derivation: crate::inference::LateBoundCallbacks,
) {
    #[cfg(any(test, feature = "test-util"))]
    let mut crash_after: Option<usize> = None;
    // ModelOutput items from one sampling call arrive before
    // ResponseEvent::Completed (token usage). Buffer them so assistant_text
    // can carry providerUsage on the same event (schema v5 / D3) without
    // reordering relative to thinking/tool_call siblings.
    let mut pending_model_output: Vec<(ResponseItem, RawItemProvenance)> = Vec::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            CaptureCmd::Persist { item, provenance } => {
                if matches!(provenance, RawItemProvenance::ModelOutput) {
                    pending_model_output.push((item, provenance));
                    continue;
                }
                if let Err(err) = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    None,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                {
                    if err == "crash" {
                        return;
                    }
                }
                if let Err(err) = persist_item(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &item,
                    provenance,
                    None,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                {
                    if err == "crash" {
                        return;
                    }
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
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                {
                    if err == "crash" {
                        return;
                    }
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
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                {
                    if err == "crash" {
                        return;
                    }
                }
                let event = map_turn_end(&thread_id, &turn_id, &reason, &facts);
                if let Err(err) = submit_mapped(&mut session, &[event]).await {
                    warn!(thread_id = %thread_id, %err, "LHC: turn_end failed");
                }
            }
            CaptureCmd::ModelOrThinkingChange {
                previous_model,
                new_model,
                previous_level,
                new_level,
            } => {
                if let Err(err) = flush_pending_model_output(
                    &mut session,
                    &mut tracker,
                    &thread_id,
                    &degraded,
                    &mut pending_model_output,
                    None,
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                {
                    if err == "crash" {
                        return;
                    }
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
                    warn!(thread_id = %thread_id, %err, "LHC: model/thinking change failed");
                }
            }
            CaptureCmd::RuntimeNote { text, key_suffix } => {
                let event = map_runtime_note(&thread_id, &text, &key_suffix);
                if let Err(err) = submit_mapped(&mut session, &[event]).await {
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
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await
                {
                    if err == "crash" {
                        return;
                    }
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
                    #[cfg(any(test, feature = "test-util"))]
                    &mut crash_after,
                )
                .await;
                close_capture_session(session, &derivation).await;
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
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
        #[cfg(any(test, feature = "test-util"))]
        &mut crash_after,
    )
    .await;
    close_capture_session(session, &derivation).await;
}

async fn flush_pending_model_output(
    session: &mut LhcSession,
    tracker: &mut OccurrenceTracker,
    thread_id: &str,
    degraded: &AtomicBool,
    pending: &mut Vec<(ResponseItem, RawItemProvenance)>,
    provider_usage: Option<&Map<String, Value>>,
    #[cfg(any(test, feature = "test-util"))] crash_after: &mut Option<usize>,
) -> Result<(), String> {
    if pending.is_empty() {
        return Ok(());
    }
    let items = std::mem::take(pending);
    for (item, provenance) in items {
        persist_item(
            session,
            tracker,
            thread_id,
            degraded,
            &item,
            provenance,
            provider_usage,
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
    #[cfg(any(test, feature = "test-util"))] crash_after: &mut Option<usize>,
) -> Result<(), String> {
    // Contain map_item panics so one bad item cannot kill the worker (H9).
    let mut local = tracker.clone();
    let mapped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        map_item(thread_id, item, provenance, &mut local)
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
            return Ok(());
        }
    };
    if let Some(usage) = provider_usage {
        for event in &mut events {
            attach_provider_usage(event, usage);
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
    if let Err(err) = submit_mapped(session, &events).await {
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
