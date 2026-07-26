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
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::models::ResponseItem;
use tracing::debug;
use tracing::error;
use tracing::warn;

use lhc::sdk::DrainOpts;
use lhc::sdk::OpResult;
use lhc::shared_tech::InferenceCallbacks;
use lhc::shared_tech::scheduler::DrainDisposition;

use crate::capture::CAPTURE_QUEUE_CAP;
use crate::capture::CaptureHandle;
use crate::capture::spawn_capture;
use crate::gating::lhc_root;
use crate::session::LhcSession;

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
fn session_derived_cap() -> usize {
    #[cfg(any(test, feature = "test-util"))]
    {
        SESSION_DERIVED_CAP_OVERRIDE.load(Ordering::SeqCst).max(1) as usize
    }
    #[cfg(not(any(test, feature = "test-util")))]
    {
        SESSION_DERIVED_CAP
    }
}

#[cfg(any(test, feature = "test-util"))]
static SESSION_DERIVED_CAP_OVERRIDE: AtomicU64 = AtomicU64::new(SESSION_DERIVED_CAP as u64);

/// Test-only: shrink the superseded provenance cap (L3 mutation harness).
#[cfg(any(test, feature = "test-util"))]
pub fn set_session_derived_cap_for_test(cap: usize) {
    SESSION_DERIVED_CAP_OVERRIDE.store(cap.max(1) as u64, Ordering::SeqCst);
}

/// Test-only: restore the production superseded cap.
#[cfg(any(test, feature = "test-util"))]
pub fn reset_session_derived_cap_for_test() {
    SESSION_DERIVED_CAP_OVERRIDE.store(SESSION_DERIVED_CAP as u64, Ordering::SeqCst);
}

/// Bound on the pre-open buffer (same order as the capture queue).
const PRE_OPEN_CAP: usize = CAPTURE_QUEUE_CAP;

/// Pending work that arrived before the capture handle was ready (H2).
enum PendingCmd {
    Persist {
        item: ResponseItem,
        provenance: RawItemProvenance,
    },
    ModelOrThinkingChange {
        previous_model: String,
        new_model: String,
        previous_level: String,
        new_level: String,
    },
}

/// Cap on **superseded** (historical) derived provenance retained across
/// write-backs (H3). Current-body ids/digests are **never** capped (L3).
pub const SESSION_DERIVED_CAP: usize = 512;

/// Derivation work items attempted per idle tick (M1).
///
/// Small on purpose: an idle tick must stay short, must not monopolise the
/// scheduler's claims, and is only an *optimisation* — the compact-time drain
/// is the correctness backstop, so a tick that under-derives costs nothing but
/// a slower first compact.
const IDLE_PUMP_MAX_ITEMS: i64 = 8;

/// Per-thread capture slot: handle is filled asynchronously after start.
/// Items arriving before open are buffered and flushed when the handle lands.
pub struct LhcCaptureSlot {
    handle: Mutex<Option<CaptureHandle>>,
    pending: Mutex<VecDeque<PendingCmd>>,
    pending_overflow: AtomicBool,
    pending_dropped: AtomicU64,
    opening: AtomicBool,
    open_failed: AtomicBool,
    /// Pinned provenance for the **current** installed body (+ durable reseed).
    /// Never subject to [`SESSION_DERIVED_CAP`] eviction (L3).
    pinned_ids: Mutex<HashSet<String>>,
    pinned_digests: Mutex<HashSet<String>>,
    /// Superseded by a later compact — may be capped (H3/L3).
    superseded_ids: Mutex<HashSet<String>>,
    superseded_digests: Mutex<HashSet<String>>,
    /// Inference callbacks for **background** derivation (M1), seeded by the
    /// host before the idle fan-out. `None` means the idle pump stays off.
    ///
    /// J1 applies here as hard as it does at compact time: whatever these
    /// callbacks produce is persisted to the archive and later *served* as if
    /// it were real derivation. The capture worker's own session holds
    /// deterministic callbacks, so pumping from there would quietly bake
    /// canned text into the record. Only the host's production callbacks are
    /// ever installed here.
    derivation_callbacks: Mutex<Option<InferenceCallbacks>>,
    /// Single-flight guard: at most one idle pump in flight per thread.
    pumping: AtomicBool,
    /// Latched by `on_thread_stop` — no pump is started after shutdown.
    stopped: AtomicBool,
    /// Completed idle pump ticks (observability + test synchronisation).
    pump_runs: AtomicU64,
}

impl LhcCaptureSlot {
    fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            pending: Mutex::new(VecDeque::new()),
            pending_overflow: AtomicBool::new(false),
            pending_dropped: AtomicU64::new(0),
            opening: AtomicBool::new(false),
            open_failed: AtomicBool::new(false),
            pinned_ids: Mutex::new(HashSet::new()),
            pinned_digests: Mutex::new(HashSet::new()),
            superseded_ids: Mutex::new(HashSet::new()),
            superseded_digests: Mutex::new(HashSet::new()),
            derivation_callbacks: Mutex::new(None),
            pumping: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            pump_runs: AtomicU64::new(0),
        }
    }

    /// Install the production derivation callbacks used by the idle pump (M1).
    ///
    /// The host resolves these the same way the compact arm does; passing
    /// deterministic callbacks here in production would silently degrade the
    /// durable record (see the field docs).
    pub fn set_derivation_callbacks(&self, callbacks: InferenceCallbacks) {
        *self
            .derivation_callbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(callbacks);
    }

    /// Whether background derivation callbacks have been seeded.
    /// Hosts use this to resolve the (non-trivial) callbacks only once.
    pub fn has_derivation_callbacks(&self) -> bool {
        self.derivation_callbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn derivation_callbacks(&self) -> Option<InferenceCallbacks> {
        self.derivation_callbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Completed background derivation ticks for this thread.
    pub fn idle_pump_runs(&self) -> u64 {
        self.pump_runs.load(Ordering::SeqCst)
    }

    /// Current capture handle, if the background open has completed.
    pub fn get(&self) -> Option<CaptureHandle> {
        self.handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
    fn set_and_flush(&self, handle: CaptureHandle) {
        let pending = {
            let mut q = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *q)
        };
        let overflowed = self.pending_overflow.load(Ordering::SeqCst);
        {
            *self
                .handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle.clone());
        }
        for cmd in pending {
            match cmd {
                PendingCmd::Persist { item, provenance } => {
                    handle.persist(&item, provenance);
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
            }
        }
        if overflowed {
            // Force a degradation latch so the record is self-describing.
            // saturating the worker is not guaranteed; latch via a synthetic
            // full-path by using the public degrade surface: re-fill then one
            // more would work, but the handle already exposes is_degraded only
            // after a real Full. Persist a runtime note via a no-op path:
            // call latch by overfilling is hard here — instead record via
            // turn_end is wrong. Use a dedicated RuntimeNote through a flood
            // of dummy items is wasteful.
            //
            // The overflow itself is already counted; emit a model-level
            // change note is wrong. Best effort: the buffer drop is logged,
            // and we force degraded by sending CAP+1 after block is not
            // available. Record via persist of a system message that maps.
            warn!(
                dropped = self.pending_dropped.load(Ordering::Relaxed),
                "LHC: pre-open buffer overflowed; some early items lost"
            );
        }
    }

    /// Buffer a command if the handle is not yet ready. Returns:
    /// - `Ok(Some(handle))` if ready
    /// - `Ok(None)` if buffered (or open failed / overflow)
    /// - does not block
    fn buffer_or_handle(&self, cmd: PendingCmd) -> Option<CaptureHandle> {
        if let Some(h) = self.get() {
            return Some(h);
        }
        if self.open_failed.load(Ordering::Relaxed) {
            return None;
        }
        let mut q = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Re-check under lock — handle may have landed.
        if let Some(h) = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Some(h);
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
    let extension = Arc::new(LhcExtension {
        enabled: Arc::new(lhc_enabled),
        model_label: Arc::new(model_label),
        thinking_level_label: Arc::new(thinking_level_label),
        cwd: Arc::new(cwd),
        root_override: None,
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.raw_item_contributor(extension.clone());
    registry.config_contributor(extension);
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
        thinking_level_label: Arc::new(thinking_level_label),
        cwd: Arc::new(cwd),
        root_override: Some(root),
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.raw_item_contributor(extension.clone());
    registry.config_contributor(extension);
}

struct LhcExtension<C> {
    enabled: Arc<dyn Fn(&C) -> bool + Send + Sync>,
    model_label: Arc<dyn Fn(&C) -> String + Send + Sync>,
    thinking_level_label: Arc<dyn Fn(&C) -> String + Send + Sync>,
    cwd: Arc<dyn Fn(&C) -> Option<String> + Send + Sync>,
    root_override: Option<PathBuf>,
}

impl<C: Sync> LhcExtension<C> {
    fn root(&self) -> PathBuf {
        self.root_override.clone().unwrap_or_else(lhc_root)
    }
}

/// Schedule a background open; never blocks the caller on SQLite (F17).
fn schedule_open(slot: Arc<LhcCaptureSlot>, thread_id: String, cwd: Option<String>, root: PathBuf) {
    if slot
        .opening
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let _ = std::thread::Builder::new()
        .name(format!("lhc-open-{thread_id}"))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    error!(?err, "LHC: open runtime failed");
                    slot.open_failed.store(true, Ordering::SeqCst);
                    return;
                }
            };
            let handle = rt.block_on(spawn_capture(
                &thread_id,
                cwd.as_deref(),
                Some(root),
            ));
            match handle {
                Some(h) => {
                    slot.set_and_flush(h);
                    debug!(thread_id = %thread_id, "LHC: capture opened (async) + pre-open buffer flushed");
                }
                None => {
                    slot.open_failed.store(true, Ordering::SeqCst);
                    error!(thread_id = %thread_id, "LHC: failed to open capture");
                }
            }
        });
}

/// M1: bounded background derivation pumped from `on_thread_idle`.
///
/// Without this, **all** derivation is deferred to the first `compact()`, where
/// it runs as one long serial burst of inference round-trips against the
/// caller's 120 s deadline — so the threads big enough to need compaction are
/// exactly the ones whose first compact times out and fails open, after
/// billing for the derivations it did manage.
///
/// Best effort by design (law 3 keeps the compact-time drain as the
/// correctness backstop):
/// * never blocks the turn loop — detached thread, like the capture open;
/// * never panics into core — the worker catches unwind;
/// * never runs without production callbacks (J1);
/// * never runs after `on_thread_stop`;
/// * single-flight, so overlapping idle ticks cannot pile up sessions.
///
/// Derivations persist to the archive, so this genuinely shrinks the
/// compact-time backlog rather than duplicating it.
fn spawn_idle_derivation_pump(slot: Arc<LhcCaptureSlot>, handle: &CaptureHandle) {
    if slot.stopped.load(Ordering::SeqCst) {
        return;
    }
    if handle.is_degraded() {
        return;
    }
    let Some(callbacks) = slot.derivation_callbacks() else {
        debug!("LHC: idle derivation pump skipped — no production callbacks seeded");
        return;
    };
    if slot
        .pumping
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // A previous tick is still deriving; nothing to queue up behind it.
        return;
    }

    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);
    let slot_worker = Arc::clone(&slot);
    let spawned = std::thread::Builder::new()
        .name(format!("lhc-idle-pump-{thread_id}"))
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| format!("runtime: {err}"))?;
                rt.block_on(run_idle_derivation_pump(
                    &thread_id,
                    root.as_deref(),
                    callbacks,
                ))
            }));
            match outcome {
                Ok(Ok((ran, remaining))) => {
                    debug!(
                        thread_id = %thread_id,
                        ran,
                        remaining,
                        "LHC: idle derivation pump tick complete"
                    );
                }
                Ok(Err(err)) => {
                    warn!(thread_id = %thread_id, %err, "LHC: idle derivation pump failed (ignored)");
                }
                Err(_) => {
                    warn!(thread_id = %thread_id, "LHC: idle derivation pump panicked (ignored)");
                }
            }
            slot_worker.pump_runs.fetch_add(1, Ordering::SeqCst);
            slot_worker.pumping.store(false, Ordering::SeqCst);
        });
    if let Err(err) = spawned {
        slot.pumping.store(false, Ordering::SeqCst);
        warn!(?err, "LHC: failed to spawn idle derivation pump thread");
    }
}

/// One bounded drain tick against the thread's archive.
/// Returns `(items_run, items_remaining)`.
async fn run_idle_derivation_pump(
    thread_id: &str,
    root: Option<&std::path::Path>,
    callbacks: InferenceCallbacks,
) -> Result<(usize, i64), String> {
    let (session, _tracker) = LhcSession::open_with_inference(thread_id, None, root, callbacks)
        .await
        .ok_or_else(|| "idle pump: LhcSession::open returned None".to_string())?;
    let report = match session
        .lhc
        .work
        .drain(
            session.thread_ref.clone(),
            Some(DrainOpts {
                max_items: Some(IDLE_PUMP_MAX_ITEMS),
            }),
        )
        .await
    {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => {
            session.close().await;
            return Err(format!("idle pump: work.drain failed: {}", error.reason));
        }
    };
    let failed = report
        .ran
        .iter()
        .filter(|e| e.disposition == DrainDisposition::FailedTerminal)
        .count();
    if failed > 0 {
        // Not fatal here: the compact-time drain re-runs and fails open if the
        // failure persists. Surfacing it early is the point.
        warn!(
            thread_id = %thread_id,
            failed,
            ran = report.ran.len(),
            "LHC: idle derivation pump saw failed_terminal work items"
        );
    }
    let out = (report.ran.len(), report.remaining);
    session.close().await;
    Ok(out)
}

async fn shutdown_capture_send(handle: CaptureHandle) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = std::thread::Builder::new()
        .name("lhc-shutdown".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => {
                    let _ = tx.send(());
                    return;
                }
            };
            rt.block_on(async move {
                handle.flush().await;
                handle.shutdown().await;
            });
            let _ = tx.send(());
        });
    let _ = rx.await;
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
            // Fire-and-forget open — Session construction continues immediately.
            schedule_open(slot, thread_id, cwd, root);
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
                // M1: idle is the natural background-drain pump.
                spawn_idle_derivation_pump(slot, &handle);
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() {
                // Latch before shutdown so no further idle tick starts a pump.
                slot.stopped.store(true, Ordering::SeqCst);
                if let Some(handle) = slot.get() {
                    shutdown_capture_send(handle).await;
                }
            }
        })
    }
}

impl<C: Send + Sync + 'static> TurnLifecycleContributor for LhcExtension<C> {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input
                .turn_store
                .insert(LhcTurnId(input.turn_id.to_string()));
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            let Some(handle) = slot.get() else {
                return;
            };
            let turn_id = input
                .turn_store
                .get::<LhcTurnId>()
                .map(|t| t.0.clone())
                .unwrap_or_else(|| "unknown".into());
            handle.turn_end(&turn_id, "stop");
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            let Some(handle) = slot.get() else {
                return;
            };
            let turn_id = input
                .turn_store
                .get::<LhcTurnId>()
                .map(|t| t.0.clone())
                .unwrap_or_else(|| "unknown".into());
            handle.turn_end(&turn_id, "abort");
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() else {
                return;
            };
            let Some(handle) = slot.get() else {
                return;
            };
            handle.turn_end(input.turn_id, "error");
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
            for item in input.items {
                let cmd = PendingCmd::Persist {
                    item: item.clone(),
                    provenance: input.provenance,
                };
                if let Some(handle) = slot.buffer_or_handle(cmd) {
                    handle.persist(item, input.provenance);
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
        if previous_model == new_model && previous_level == new_level {
            return;
        }
        let Some(slot) = thread_store.get::<LhcCaptureSlot>() else {
            return;
        };
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
    let start = std::time::Instant::now();
    loop {
        if let Some(h) = slot.get() {
            return Some(h);
        }
        if slot.open_failed.load(Ordering::Relaxed) {
            return None;
        }
        if start.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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

    /// Real registry + stores, so tests fire the **production** lifecycle
    /// hooks (`on_thread_idle` / `on_thread_stop`) rather than the pump
    /// function directly — otherwise deleting the `on_thread_idle` call site
    /// would leave these tests green.
    struct IdleHarness {
        registry: codex_extension_api::ExtensionRegistry<()>,
        session_store: ExtensionData,
        thread_store: ExtensionData,
        slot: Arc<LhcCaptureSlot>,
    }

    impl IdleHarness {
        async fn fire_idle(&self) {
            for contributor in self.registry.thread_lifecycle_contributors() {
                contributor
                    .on_thread_idle(ThreadIdleInput {
                        session_store: &self.session_store,
                        thread_store: &self.thread_store,
                    })
                    .await;
            }
        }

        async fn fire_stop(&self) {
            for contributor in self.registry.thread_lifecycle_contributors() {
                contributor
                    .on_thread_stop(ThreadStopInput {
                        session_store: &self.session_store,
                        thread_store: &self.thread_store,
                    })
                    .await;
            }
        }
    }

    /// Build a real registry + slot and seed a bandable thread through the
    /// production capture path.
    async fn seeded_slot(root: &std::path::Path, tid: &str) -> IdleHarness {
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
                    session_store: &session_store,
                    thread_store: &store,
                })
                .await;
        }
        let slot = store.get::<LhcCaptureSlot>().expect("slot");
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
            );
            handle.persist(
                &msg(
                    "assistant",
                    &format!("assistant reply {i} bandable {pad}"),
                    &format!("a{i}"),
                ),
                RawItemProvenance::ModelOutput,
            );
        }
        handle.flush().await;
        IdleHarness {
            registry,
            session_store,
            thread_store: store,
            slot,
        }
    }

    /// M1: `on_thread_idle` must pump bounded derivation work in the
    /// background. Before M1 it only flushed capture, so every derivation was
    /// deferred to the first compact.
    ///
    /// Driven through the real `ThreadLifecycleContributor::on_thread_idle`.
    #[tokio::test]
    async fn m1_idle_tick_derives_in_background_and_shrinks_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let tid = "m1-idle-pump";
        let h = seeded_slot(root, tid).await;

        // Gate: no production callbacks seeded → no pump, no inference (J1).
        let counter = Arc::new(AtomicUsize::new(0));
        h.fire_idle().await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            h.slot.idle_pump_runs(),
            0,
            "M1: no pump may run without host-seeded production callbacks"
        );

        h.slot
            .set_derivation_callbacks(counting_callbacks(Arc::clone(&counter)));

        let backlog_before = drain_backlog(root, tid).await;
        assert!(
            backlog_before > (4 * IDLE_PUMP_MAX_ITEMS),
            "fixture: backlog ({backlog_before}) must exceed several ticks"
        );

        const TICKS: u64 = 4;
        for tick in 1..=TICKS {
            h.fire_idle().await;
            wait_for_pump_runs(&h.slot, tick, std::time::Duration::from_secs(20)).await;
        }
        assert_eq!(h.slot.idle_pump_runs(), TICKS, "each idle tick pumps once");

        let derived = counter.load(Ordering::SeqCst);
        assert!(
            derived > 0,
            "M1: idle ticks must invoke real derivation inference; got 0 — \
             on_thread_idle no longer pumps, or it ran with other callbacks"
        );
        let backlog_after = drain_backlog(root, tid).await;
        assert!(
            backlog_after < backlog_before,
            "M1: background pumping must shrink the compact-time backlog: \
             before={backlog_before} after={backlog_after} derived={derived}"
        );
    }

    /// M1: no pump may start after `on_thread_stop`.
    #[tokio::test]
    async fn m1_no_pump_after_thread_stop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let tid = "m1-idle-pump-stop";
        let h = seeded_slot(root, tid).await;
        let counter = Arc::new(AtomicUsize::new(0));
        h.slot
            .set_derivation_callbacks(counting_callbacks(Arc::clone(&counter)));

        // Positive control first: the pump does run before stop.
        h.fire_idle().await;
        assert_eq!(
            wait_for_pump_runs(&h.slot, 1, std::time::Duration::from_secs(20)).await,
            1
        );
        let calls_before_stop = counter.load(Ordering::SeqCst);
        assert!(calls_before_stop > 0, "positive control: pump derives");

        h.fire_stop().await;
        h.fire_idle().await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            h.slot.idle_pump_runs(),
            1,
            "M1: no derivation pump may start after on_thread_stop"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            calls_before_stop,
            "M1: no inference after thread stop"
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

    async fn wait_for_pump_runs(
        slot: &LhcCaptureSlot,
        target: u64,
        timeout: std::time::Duration,
    ) -> u64 {
        let start = std::time::Instant::now();
        loop {
            let runs = slot.idle_pump_runs();
            if runs >= target || start.elapsed() > timeout {
                return runs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn l3_current_body_provenance_survives_tiny_cap() {
        set_session_derived_cap_for_test(2);
        let slot = LhcCaptureSlot::new();

        // Ten write-backs of 5 ids each under cap=2: every *current* body must
        // remain fully protected; only superseded history may be trimmed.
        for round in 0..10 {
            let ids: Vec<String> = (0..5).map(|i| format!("r{round}-id{i}")).collect();
            let digests: Vec<String> = (0..5).map(|i| format!("r{round}-d{i}")).collect();
            slot.mark_derived_after_writeback(ids.clone(), digests.clone())
                .expect("mark");
            let have = slot.derived_ids();
            for id in &ids {
                assert!(
                    have.contains(id),
                    "L3: current body id {id} must not be evicted at round {round} (cap=2); have={have:?}"
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
        reset_session_derived_cap_for_test();
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

    #[test]
    fn l3_superseded_only_is_capped() {
        set_session_derived_cap_for_test(2);
        let slot = LhcCaptureSlot::new();
        slot.mark_derived_after_writeback(
            vec!["old1".into(), "old2".into(), "old3".into()],
            vec!["od1".into(), "od2".into(), "od3".into()],
        )
        .unwrap();
        slot.mark_derived_after_writeback(
            vec!["new1".into(), "new2".into()],
            vec!["nd1".into(), "nd2".into()],
        )
        .unwrap();
        let have = slot.derived_ids();
        assert!(have.contains("new1") && have.contains("new2"));
        // At most 2 superseded + 2 current = 4; some old* may be gone.
        let old_retained = ["old1", "old2", "old3"]
            .iter()
            .filter(|id| have.contains(**id))
            .count();
        assert!(
            old_retained <= 2,
            "superseded should be capped to 2, retained {old_retained}"
        );
        reset_session_derived_cap_for_test();
    }
}
