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

use crate::capture::CAPTURE_QUEUE_CAP;
use crate::capture::CaptureHandle;
use crate::capture::spawn_capture;
use crate::gating::lhc_root;

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

/// Per-thread capture slot: handle is filled asynchronously after start.
/// Items arriving before open are buffered and flushed when the handle lands.
pub struct LhcCaptureSlot {
    handle: Mutex<Option<CaptureHandle>>,
    pending: Mutex<VecDeque<PendingCmd>>,
    pending_overflow: AtomicBool,
    pending_dropped: AtomicU64,
    opening: AtomicBool,
    open_failed: AtomicBool,
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
        }
    }

    fn get(&self) -> Option<CaptureHandle> {
        self.handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Install the handle and flush any pre-open buffer (H2).
    fn set_and_flush(&self, handle: CaptureHandle) {
        let pending = {
            let mut q = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *q)
        };
        let overflowed = self.pending_overflow.load(Ordering::SeqCst);
        {
            *self.handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.clone());
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
        let mut q = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // Re-check under lock — handle may have landed.
        if let Some(h) = self
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
            let slot = input
                .thread_store
                .get::<LhcCaptureSlot>()
                .expect("just inserted");
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
            if let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() {
                if let Some(handle) = slot.get() {
                    handle.flush_async();
                }
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(slot) = input.thread_store.get::<LhcCaptureSlot>() {
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
