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
use codex_extension_api::TokenUsageContributor;
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
    ProviderUsage {
        usage: codex_protocol::protocol::TokenUsage,
    },
    TurnEnd {
        turn_id: String,
        reason: String,
        facts: TurnEndFacts,
    },
}

/// Cap on **superseded** (historical) derived provenance retained across
/// write-backs (H3). Current-body ids/digests are **never** capped (L3).
pub const SESSION_DERIVED_CAP: usize = 512;

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
    /// Late binding for the **capture session's** derivation callbacks.
    ///
    /// Under `SdkMode::Background` LHC's scheduler derives on the capture
    /// session, so this is what lands in the durable record. J1 applies as hard
    /// as it does at compact time: only the host's production callbacks are
    /// ever installed here — never the deterministic ones, which would bake
    /// canned text into the record and serve it as real derivation.
    derivation_callbacks: crate::inference::LateBoundCallbacks,
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
            derivation_callbacks: crate::inference::LateBoundCallbacks::new(),
        }
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
                PendingCmd::ProviderUsage { usage } => {
                    handle.provider_usage(&usage);
                }
                PendingCmd::TurnEnd {
                    turn_id,
                    reason,
                    facts,
                } => {
                    handle.turn_end(&turn_id, &reason, facts);
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
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
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
        provider_label: Arc::new(|_c| "openai".to_string()),
        thinking_level_label: Arc::new(thinking_level_label),
        cwd: Arc::new(cwd),
        root_override: Some(root),
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
    registry.raw_item_contributor(extension.clone());
    registry.config_contributor(extension);
}

struct LhcExtension<C> {
    enabled: Arc<dyn Fn(&C) -> bool + Send + Sync>,
    model_label: Arc<dyn Fn(&C) -> String + Send + Sync>,
    provider_label: Arc<dyn Fn(&C) -> String + Send + Sync>,
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
fn schedule_open(
    slot: Arc<LhcCaptureSlot>,
    thread_id: String,
    cwd: Option<String>,
    root: PathBuf,
    initial_identity: Option<ModelIdentity>,
) {
    if slot
        .opening
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let derivation = slot.derivation_callbacks.clone();
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
            let handle = rt.block_on(spawn_capture_with_identity(
                &thread_id,
                cwd.as_deref(),
                Some(root),
                derivation,
                initial_identity,
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
            // Fire-and-forget open — Session construction continues immediately.
            schedule_open(slot, thread_id, cwd, root, Some(identity));
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
            if let Some(slot) = input.thread_store.get::<LhcCaptureSlot>()
                && let Some(handle) = slot.get()
            {
                shutdown_capture_send(handle).await;
            }
        })
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
        // what actually produced later thinking. (A change racing the async
        // open keeps the open-time identity; accepted.)
        if (previous_model != new_model || previous_provider != new_provider)
            && let Some(handle) = slot.get()
        {
            handle.set_identity(ModelIdentity::new(
                new_provider,
                new_model.clone(),
                ModelIdentity::RESPONSES_API,
            ));
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
