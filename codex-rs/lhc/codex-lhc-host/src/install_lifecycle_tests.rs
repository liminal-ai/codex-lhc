//! Capture-slot lifecycle proofs (LIM-134).
//!
//! The slot has one watchable lifecycle — `Opening` → `Ready` / `Failed` /
//! `Stopped` — every transition wakes every waiter, and **every** exit of the
//! dedicated open thread terminalizes the slot.
//!
//! Ordering here is proved two ways, never by hoping a sleep was long enough:
//! - the lifecycle receiver is taken *before* the transition, so "the waiter
//!   cannot miss it" is a structural fact rather than a scheduling race;
//! - where a wait must be shown to actually block, the tokio clock is paused
//!   and advanced, so the elapsed time is virtual and exact.
//!
//! Timeouts appear only as deadlock ceilings.

use super::*;

use crate::capture::spawn_capture_with_identity;
use crate::inference::LateBoundCallbacks;
use crate::inference::lhc_inference_callbacks;
use crate::mapping::ModelIdentity;
use crate::parse_rollout_items;
use crate::rollout_reconcile::RolloutReconcileTrigger;
use crate::rollout_reconcile::regenerate_rollout_from_thread;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use std::time::Duration;
use tempfile::tempdir;

/// Deadlock ceiling only — never the evidence that ordering happened.
const DEADLOCK_CEILING: Duration = Duration::from_secs(30);

fn user_msg(text: &str, id: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.into())),
        role: "user".into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn is_payload(item: &codex_history::RolloutItem, text: &str) -> bool {
    match item {
        codex_history::RolloutItem::ResponseItem(item) => match &item.item {
            ResponseItem::Message { role, content, .. } if role == "user" => content
                .iter()
                .any(|c| matches!(c, ContentItem::InputText { text: t } if t == text)),
            _ => false,
        },
        _ => false,
    }
}

async fn open_handle(root: &std::path::Path, tid: &str) -> CaptureHandle {
    let derivation = LateBoundCallbacks::new();
    derivation.seed(lhc_inference_callbacks(false).expect("deterministic callbacks"));
    spawn_capture_with_identity(
        tid,
        None,
        Some(root.to_path_buf()),
        derivation,
        Some(ModelIdentity::new(
            "openai",
            "gpt-a",
            ModelIdentity::RESPONSES_API,
        )),
    )
    .await
    .expect("capture")
}

async fn persisted_payloads(
    dir: &std::path::Path,
    root: &std::path::Path,
    tid: &str,
    text: &str,
) -> usize {
    let path = dir.join(format!("{tid}.jsonl"));
    regenerate_rollout_from_thread(
        &path,
        tid,
        Some(root),
        RolloutReconcileTrigger::Missing,
        None,
    )
    .await
    .expect("regenerate thread");
    parse_rollout_items(&path)
        .expect("parse rollout")
        .iter()
        .filter(|item| is_payload(item, text))
        .count()
}

/// A registry + stores wired the way `on_thread_start` leaves them.
struct SlotHarness {
    registry: ExtensionRegistry<()>,
    session_store: ExtensionData,
    thread_store: ExtensionData,
}

impl SlotHarness {
    /// Install the production contributors without starting a thread, so the
    /// test owns when (and whether) the open is scheduled.
    fn detached(root: std::path::PathBuf, thread_id: &str) -> Self {
        let mut builder = ExtensionRegistryBuilder::<()>::new();
        install_with_root(&mut builder, |_config| true, root);
        Self::from_builder(builder, thread_id, /*insert_slot*/ true)
    }

    /// Install the production contributors with a fault armed on the dedicated
    /// open thread. The slot is created by the real `on_thread_start`.
    fn with_open_fault(root: std::path::PathBuf, thread_id: &str, fault: OpenThreadFault) -> Self {
        let mut builder = ExtensionRegistryBuilder::<()>::new();
        install_with_root_and_open_fault(&mut builder, |_config| true, root, fault);
        Self::from_builder(builder, thread_id, /*insert_slot*/ false)
    }

    fn from_builder(
        builder: ExtensionRegistryBuilder<()>,
        thread_id: &str,
        insert_slot: bool,
    ) -> Self {
        let harness = Self {
            registry: builder.build(),
            session_store: ExtensionData::new("session"),
            thread_store: ExtensionData::new(thread_id),
        };
        if insert_slot {
            harness.thread_store.insert(LhcCaptureSlot::new());
        }
        harness
    }

    fn slot(&self) -> Arc<LhcCaptureSlot> {
        self.thread_store
            .get::<LhcCaptureSlot>()
            .expect("capture slot")
    }

    /// Run the real `on_thread_start` contributors (schedules the open).
    async fn start_thread(&self) {
        let config = ();
        let session_source = SessionSource::Exec;
        let environments = [];
        for contributor in self.registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_start(ThreadStartInput {
                    config: &config,
                    session_source: &session_source,
                    persistent_thread_state_available: false,
                    environments: &environments,
                    mcp_resource_client: None,
                    extension_metrics: None,
                    session_store: &self.session_store,
                    thread_store: &self.thread_store,
                })
                .await;
        }
    }

    async fn stop_thread(&self) {
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

// ---------------------------------------------------------------------------
// Transitions wake waiters
// ---------------------------------------------------------------------------

/// Every waiter that subscribed while `Opening` observes `Ready` — including
/// one whose subscription raced the publication.
#[tokio::test]
async fn opening_to_ready_wakes_every_waiter() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let slot = Arc::new(LhcCaptureSlot::new());
    let handle = open_handle(&root, "lifecycle-ready").await;

    // Subscribing before the publication is what makes this race-free: the
    // waiter tasks need not have been polled yet.
    let waiters = (0..4)
        .map(|_| {
            let rx = slot.subscribe_lifecycle();
            let slot = Arc::clone(&slot);
            tokio::spawn(async move { slot.await_settled_on(rx, DEADLOCK_CEILING).await })
        })
        .collect::<Vec<_>>();

    assert!(slot.set_and_flush(handle.clone()), "Ready must publish");

    for waiter in waiters {
        let settled = waiter.await.expect("waiter join");
        assert!(
            matches!(settled, Some(CaptureState::Ready(_))),
            "waiter must observe Ready, got {settled:?}"
        );
    }
    handle.shutdown().await;
}

/// A permanent open failure ends the readiness wait with a stable reason, and
/// retrieval distinguishes it from "still opening".
#[tokio::test]
async fn opening_to_failed_wakes_waiters_with_stable_reason() {
    let slot = Arc::new(LhcCaptureSlot::new());
    let rx = slot.subscribe_lifecycle();
    let waiter = tokio::spawn({
        let slot = Arc::clone(&slot);
        async move { slot.await_settled_on(rx, DEADLOCK_CEILING).await }
    });

    assert!(slot.publish_failed(CAPTURE_OPEN_FAILED));

    let settled = waiter.await.expect("waiter join");
    assert!(
        matches!(&settled, Some(CaptureState::Failed(reason)) if reason == CAPTURE_OPEN_FAILED),
        "waiter must observe Failed with the stable reason, got {settled:?}"
    );
    assert_eq!(
        slot.resolve_for_retrieval().err(),
        Some(RetrievalLifecycleError::OpenFailed),
        "retrieval must distinguish Failed from still Opening"
    );
}

/// Thread stop while still `Opening` ends the readiness wait as `Stopped`.
#[tokio::test]
async fn opening_to_stopped_wakes_waiters() {
    let slot = Arc::new(LhcCaptureSlot::new());
    let rx = slot.subscribe_lifecycle();
    let waiter = tokio::spawn({
        let slot = Arc::clone(&slot);
        async move { slot.await_settled_on(rx, DEADLOCK_CEILING).await }
    });

    slot.stop_and_drop_pending();

    let settled = waiter.await.expect("waiter join");
    assert!(
        matches!(settled, Some(CaptureState::Stopped)),
        "waiter must observe Stopped, got {settled:?}"
    );
    assert_eq!(
        slot.resolve_for_retrieval().err(),
        Some(RetrievalLifecycleError::Shutdown),
        "retrieval must distinguish Stopped from still Opening"
    );
}

// ---------------------------------------------------------------------------
// Every open-thread exit terminalizes the slot
// ---------------------------------------------------------------------------

/// A failed native thread spawn is not silence: the production scheduler
/// publishes `Failed` and the parked waiter is released.
#[tokio::test]
async fn open_thread_spawn_failure_wakes_waiters_as_failed() {
    let dir = tempdir().unwrap();
    let harness = SlotHarness::with_open_fault(
        dir.path().join("lhc"),
        "spawn-failure",
        OpenThreadFault::SpawnFailed,
    );

    harness.start_thread().await;
    let slot = harness.slot();

    let settled = slot.await_settled(DEADLOCK_CEILING).await;
    assert!(
        matches!(&settled, Some(CaptureState::Failed(reason))
            if reason == CAPTURE_OPEN_THREAD_UNAVAILABLE),
        "a failed open-thread spawn must terminalize the slot: {settled:?}"
    );
}

/// An unwind inside the open thread before it settles the slot must not leave
/// readiness waiters parked forever: the scoped completion guard publishes
/// `Failed` on the way out.
#[tokio::test]
async fn open_thread_panic_before_settlement_wakes_waiters_as_failed() {
    let dir = tempdir().unwrap();
    let harness = SlotHarness::with_open_fault(
        dir.path().join("lhc"),
        "panic-before-settle",
        OpenThreadFault::PanicBeforeSettle,
    );

    // The injected panic unwinds on the dedicated open thread; the scoped
    // completion guard's Drop is the production mechanism under test.
    harness.start_thread().await;
    let slot = harness.slot();

    let settled = slot.await_settled(DEADLOCK_CEILING).await;
    assert!(
        matches!(&settled, Some(CaptureState::Failed(reason))
            if reason == CAPTURE_OPEN_ABANDONED),
        "an abandoned open must terminalize the slot: {settled:?}"
    );
}

// ---------------------------------------------------------------------------
// Thread stop during Opening
// ---------------------------------------------------------------------------

/// Thread stop that races an in-flight open. The stop is shown to actually be
/// waiting (virtual time advances inside the bound with the contributor still
/// pending), and when `Ready` lands the ordered pre-open buffer still replays
/// and the bounded capture shutdown runs.
#[tokio::test]
async fn stop_during_opening_replays_and_shuts_down_when_ready_arrives() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "lifecycle-stop-ready";
    let harness = Arc::new(SlotHarness::detached(root.clone(), tid));
    let slot = harness.slot();

    // Buffer a command while the slot is still Opening.
    assert!(
        slot.buffer_or_handle(PendingCmd::Persist {
            item: user_msg("carried", "u-carried"),
            provenance: RawItemProvenance::UserPrompt,
            step_index: None,
            steer: false,
        })
        .is_none(),
        "pre-open command must buffer"
    );
    let handle = open_handle(&root, tid).await;

    let stop = {
        let harness = Arc::clone(&harness);
        tokio::spawn(async move { harness.stop_thread().await })
    };

    // Virtual time: advance strictly inside the shutdown bound. Auto-advance
    // only moves the clock once every task is parked, so reaching here with
    // `stop` unfinished proves the contributor is genuinely waiting.
    tokio::time::pause();
    tokio::time::advance(CAPTURE_SHUTDOWN_BOUND / 2).await;
    assert!(
        !stop.is_finished(),
        "on_thread_stop must wait for an open that is still in flight"
    );
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "the contributor must not give up inside the bound"
    );
    tokio::time::resume();

    assert!(
        slot.set_and_flush(handle.clone()),
        "Ready must publish inside the shutdown bound"
    );
    tokio::time::timeout(DEADLOCK_CEILING, stop)
        .await
        .expect("stop must return once the open settles")
        .expect("stop join");

    assert!(
        matches!(slot.state(), CaptureState::Stopped),
        "stop is terminal once the handle has been torn down"
    );
    assert_eq!(
        persisted_payloads(dir.path(), &root, tid, "carried").await,
        1,
        "a Ready that lands inside the bound must still replay the pre-open buffer"
    );
}

/// Bound expiry through the real contributor: the slot goes `Stopped`, the
/// buffered commands are dropped once, and the open thread's late `Ready` is
/// suppressed without replaying anything.
#[tokio::test]
async fn stop_bound_expiry_suppresses_late_ready_and_drops_buffer() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "lifecycle-late-ready";
    let harness = Arc::new(SlotHarness::detached(root.clone(), tid));
    let slot = harness.slot();
    for i in 0..3 {
        assert!(
            slot.buffer_or_handle(PendingCmd::Persist {
                item: user_msg("abandoned", &format!("u{i}")),
                provenance: RawItemProvenance::UserPrompt,
                step_index: None,
                steer: false,
            })
            .is_none()
        );
    }

    let stop = {
        let harness = Arc::clone(&harness);
        tokio::spawn(async move { harness.stop_thread().await })
    };

    // Expire the bound in virtual time — exact, and zero wall-clock cost.
    tokio::time::pause();
    tokio::time::advance(CAPTURE_SHUTDOWN_BOUND / 2).await;
    assert!(!stop.is_finished(), "the contributor must still be waiting");
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "the contributor must not give up before the bound expires"
    );
    tokio::time::advance(CAPTURE_SHUTDOWN_BOUND).await;
    tokio::time::timeout(DEADLOCK_CEILING, stop)
        .await
        .expect("stop must return once the bound expires")
        .expect("stop join");
    tokio::time::resume();

    assert!(
        matches!(slot.state(), CaptureState::Stopped),
        "bound expiry leaves the slot terminal"
    );
    assert_eq!(
        slot.stop_and_drop_pending(),
        0,
        "the contributor already dropped the buffer exactly once"
    );

    // The dedicated open thread finishes its inline SQLite call afterwards.
    let handle = open_handle(&root, tid).await;
    assert!(
        !slot.set_and_flush(handle.clone()),
        "a late open must not publish over a stopped slot"
    );
    assert!(slot.get().is_none(), "no handle is observable after stop");

    handle.flush().await;
    handle.shutdown().await;
    assert_eq!(
        persisted_payloads(dir.path(), &root, tid, "abandoned").await,
        0,
        "commands dropped at stop must never replay onto a late handle"
    );
}

/// Timeout-vs-Ready cell: `await_settled` returns `None` while the slot is
/// already `Ready`. Re-read must take the Ready path — bounded handle
/// shutdown, no later publish, buffer flushed once and not also dropped.
#[tokio::test]
async fn stop_timeout_reread_shuts_down_ready_that_landed_in_the_window() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "lifecycle-timeout-ready";
    let harness = SlotHarness::detached(root.clone(), tid);
    let slot = harness.slot();
    assert!(
        slot.buffer_or_handle(PendingCmd::Persist {
            item: user_msg("raced", "u-raced"),
            provenance: RawItemProvenance::UserPrompt,
            step_index: None,
            steer: false,
        })
        .is_none(),
        "pre-open command must buffer"
    );
    let handle = open_handle(&root, tid).await;
    assert!(
        slot.set_and_flush(handle.clone()),
        "Ready must publish before the timeout re-read"
    );
    assert!(
        matches!(slot.state(), CaptureState::Ready(_)),
        "fixture: the slot is Ready when the wait reports timeout"
    );

    apply_thread_stop(&slot, /*settled*/ None).await;

    assert!(
        matches!(slot.state(), CaptureState::Stopped),
        "stop is terminal after the Ready-path shutdown"
    );
    assert!(slot.get().is_none(), "no handle is observable after stop");
    assert_eq!(
        slot.stop_and_drop_pending(),
        0,
        "Ready-path stop must not also drop the already-flushed buffer"
    );
    assert_eq!(
        persisted_payloads(dir.path(), &root, tid, "raced").await,
        1,
        "a Ready that landed in the timeout window must flush the buffer exactly once"
    );
    handle.persist(
        &user_msg("after-stop", "u-after"),
        RawItemProvenance::UserPrompt,
        None,
    );
    let _ = handle.flush_within(DEADLOCK_CEILING).await;
    assert_eq!(
        persisted_payloads(dir.path(), &root, tid, "after-stop").await,
        0,
        "a shut-down handle must not persist after Stopped"
    );

    let late = open_handle(&root, "lifecycle-timeout-ready-late").await;
    assert!(
        !slot.set_and_flush(late.clone()),
        "no publish after Stopped"
    );
    late.flush().await;
    late.shutdown().await;
}

/// Real contributor: Ready lands at the same virtual instant as bound expiry.
/// Either the wait observes Ready or the timeout re-read does — the handle
/// is shut down, the buffer is flushed xor dropped once, never both/neither.
#[tokio::test]
async fn stop_bound_expiry_concurrent_with_ready_shuts_down_handle() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "lifecycle-expiry-ready";
    let harness = Arc::new(SlotHarness::detached(root.clone(), tid));
    let slot = harness.slot();
    assert!(
        slot.buffer_or_handle(PendingCmd::Persist {
            item: user_msg("concurrent", "u-concurrent"),
            provenance: RawItemProvenance::UserPrompt,
            step_index: None,
            steer: false,
        })
        .is_none()
    );
    let handle = open_handle(&root, tid).await;

    let stop = {
        let harness = Arc::clone(&harness);
        tokio::spawn(async move { harness.stop_thread().await })
    };

    tokio::time::pause();
    tokio::time::advance(CAPTURE_SHUTDOWN_BOUND / 2).await;
    assert!(!stop.is_finished(), "on_thread_stop must still be waiting");
    tokio::time::advance(CAPTURE_SHUTDOWN_BOUND).await;
    // Ready is published at the same virtual instant the bound expires,
    // before the contributor is polled again.
    assert!(
        slot.set_and_flush(handle.clone()),
        "Ready must still publish: stop has not yet consumed the slot"
    );
    tokio::time::timeout(DEADLOCK_CEILING, stop)
        .await
        .expect("stop must return once Ready or the bound is observed")
        .expect("stop join");
    tokio::time::resume();

    assert!(
        matches!(slot.state(), CaptureState::Stopped),
        "stop is terminal"
    );
    assert!(slot.get().is_none(), "no handle is observable after stop");
    let dropped = slot.stop_and_drop_pending();
    let flushed = persisted_payloads(dir.path(), &root, tid, "concurrent").await;
    assert!(
        (flushed == 1 && dropped == 0) || (flushed == 0 && dropped == 0),
        "buffer flushed exactly once or dropped exactly once, never both: flushed={flushed} dropped={dropped}"
    );
    // `dropped == 0` after stop is "already consumed" — either flushed at
    // Ready or dropped inside the contributor. Neither-nor would leave the
    // command buffered; the extra stop_and_drop_pending would then return >0.
    assert_eq!(
        dropped, 0,
        "the contributor must have flushed or dropped the buffer; a leftover queue is neither"
    );
    handle.persist(
        &user_msg("after-stop", "u-after-concurrent"),
        RawItemProvenance::UserPrompt,
        None,
    );
    let _ = handle.flush_within(DEADLOCK_CEILING).await;
    assert_eq!(
        persisted_payloads(dir.path(), &root, tid, "after-stop").await,
        0,
        "the handle must be shut down whether Ready was seen by wait or re-read"
    );
}
