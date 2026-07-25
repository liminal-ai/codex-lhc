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
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::error;
use tracing::warn;

use crate::idempotency::OccurrenceTracker;
use crate::mapping::MappedEvent;
use crate::mapping::map_item;
use crate::mapping::map_model_or_thinking_change;
use crate::mapping::map_runtime_note;
use crate::mapping::map_turn_end;
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
        reason: String,
    },
    /// Model and/or thinking-level change (from ConfigContributor).
    ModelOrThinkingChange {
        previous_model: String,
        new_model: String,
        previous_level: String,
        new_level: String,
    },
    Flush(oneshot::Sender<()>),
    #[cfg(any(test, feature = "test-util"))]
    ListEvents(oneshot::Sender<Result<Vec<lhc::intake_stream::EventRecord>, String>>),
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

    pub fn turn_end(&self, turn_id: &str, reason: &str) {
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
) -> Option<CaptureHandle> {
    let (session, tracker) = LhcSession::open(thread_id, cwd, root.as_deref()).await?;
    let (tx, rx) = mpsc::channel(CAPTURE_QUEUE_CAP);
    let dropped = Arc::new(AtomicU64::new(0));
    let degraded = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(CaptureShared {
        thread_id: thread_id.to_string(),
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
                    worker_loop(session, tracker, rx, thread_id_owned, degraded_worker).await;
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
) {
    #[cfg(any(test, feature = "test-util"))]
    let mut crash_after: Option<usize> = None;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            CaptureCmd::Persist { item, provenance } => {
                // Contain map_item panics so one bad item cannot kill the worker (H9).
                let mut local = tracker.clone();
                let mapped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    map_item(&thread_id, &item, provenance, &mut local)
                }));
                let events = match mapped {
                    Ok(events) => {
                        tracker = local;
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
                        continue;
                    }
                };
                if events.is_empty() {
                    continue;
                }
                #[cfg(any(test, feature = "test-util"))]
                if let Some(after) = crash_after.take() {
                    if after == 0 {
                        error!(thread_id = %thread_id, "LHC: crash injection before persist");
                        return;
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
                    return;
                }
                if let Err(err) = submit_mapped(&mut session, &events).await {
                    warn!(thread_id = %thread_id, %err, "LHC: persist failed");
                    if session.capture_disabled {
                        degraded.store(true, Ordering::SeqCst);
                        error!(
                            thread_id = %thread_id,
                            "LHC: capture permanently disabled after repeated failures"
                        );
                        // Record a truncation note while still possible.
                        let note = map_runtime_note(
                            &thread_id,
                            "LHC capture permanently disabled after repeated submit failures",
                            "disabled",
                        );
                        let _ = submit_mapped(&mut session, &[note]).await;
                    }
                }
            }
            CaptureCmd::TurnEnd { turn_id, reason } => {
                let event = map_turn_end(&thread_id, &turn_id, &reason);
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
                let _ = ack.send(());
            }
            #[cfg(any(test, feature = "test-util"))]
            CaptureCmd::ListEvents(ack) => {
                let _ = ack.send(session.list_events().await);
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
                session.close().await;
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
                return;
            }
        }
    }
    session.close().await;
}

async fn submit_mapped(
    session: &mut LhcSession,
    events: &[MappedEvent],
) -> Result<lhc::intake_stream::BatchResult, String> {
    let inputs: Vec<_> = events.iter().map(|e| e.input.clone()).collect();
    session.submit_events(&inputs).await
}
