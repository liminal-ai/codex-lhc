//! Per-thread LHC instance / thread lifecycle.
//!
//! Capture identity lives only in LHC (registry + thread SQLite). Generation is
//! latched from `BatchResult.thread_position.last_event_order` — never a sidecar.

use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use lhc::intake_stream::EventRecord;
use lhc::sdk::Lhc;
use lhc::sdk::OpResult;
use lhc::sdk::SdkConfig;
use lhc::sdk::ThreadRef;
use lhc::sdk::init_lhc;
use lhc::shared_tech::SdkMode;
use lhc::threads::NewThreadInput;
use lhc::threads::ResolveInput;
use tokio::sync::Mutex as AsyncMutex;
use tracing::error;
use tracing::warn;

use lhc::shared_tech::InferenceCallbacks;

use crate::gating::lhc_root;
use crate::idempotency::OccurrenceTracker;
use crate::idempotency::seed_occurrence_from_keys;

/// Serialize registry schema init — concurrent `new_thread` races on CREATE TABLE.
fn registry_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

/// Live LHC capture session (owns the SDK instance + thread path).
pub struct LhcSession {
    pub thread_id: String,
    pub lhc: Lhc,
    pub thread_ref: ThreadRef,
    /// Retained for diagnostics / Chunk 2 path resolution.
    #[allow(dead_code)]
    pub file_path: PathBuf,
    /// Retained for diagnostics / Chunk 2 path resolution.
    #[allow(dead_code)]
    pub registry_path: PathBuf,
    /// Latched from LHC `last_event_order`.
    pub generation: u64,
    /// After persistent failures, further capture is disabled for this session.
    pub capture_disabled: bool,
    failure_count: u32,
}

impl LhcSession {
    /// Create or reopen on the **capture** path, in [`SdkMode::Background`].
    ///
    /// Background is what LHC is designed for: the scheduler pokes after each
    /// intake commit and drains queued derivation itself, plus a first-touch
    /// catch-up for work left by a previous process
    /// (`docs/onboard/01-core-concepts.md` §Host mode). The reference host,
    /// pi-lhc, constructs the SDK "always in background mode, regardless of
    /// caller config" (`04-host-pi-lhc.md`).
    ///
    /// **Only this path gets a scheduler**, and the reason is runtime lifetime.
    /// The scheduler drains via `tokio::spawn`, so it needs a runtime that
    /// outlives the work. The capture worker owns exactly that: a
    /// `new_current_thread` runtime driven by `block_on(worker_loop)` for the
    /// whole thread lifetime, so spawned drains interleave with the worker's
    /// awaits and survive until shutdown. Every other `LhcSession` is opened on
    /// a runtime built for one call and dropped when it returns; a scheduler
    /// there would spawn drains that are cancelled at runtime drop, leaving
    /// claimed rows to sit out their lease. Inert is strictly better.
    ///
    /// `set_scheduler_poke` / `set_thread_touch` are `thread_local!`
    /// (`shared_tech/context.rs`), not process globals, so the capture thread's
    /// hooks cannot clobber another instance's.
    pub async fn open(
        thread_id: &str,
        cwd: Option<&str>,
        root: Option<&Path>,
        callbacks: InferenceCallbacks,
    ) -> Option<(Self, OccurrenceTracker)> {
        Self::open_with_mode(thread_id, cwd, root, callbacks, SdkMode::Background).await
    }

    /// Create or reopen for a **short-lived** read or compact session, in
    /// [`SdkMode::Manual`] — see [`Self::open`] for why these do not schedule.
    pub async fn open_with_inference(
        thread_id: &str,
        cwd: Option<&str>,
        root: Option<&Path>,
        inference_callbacks: InferenceCallbacks,
    ) -> Option<(Self, OccurrenceTracker)> {
        Self::open_with_mode(thread_id, cwd, root, inference_callbacks, SdkMode::Manual).await
    }

    async fn open_with_mode(
        thread_id: &str,
        cwd: Option<&str>,
        root: Option<&Path>,
        inference_callbacks: InferenceCallbacks,
        mode: SdkMode,
    ) -> Option<(Self, OccurrenceTracker)> {
        let root_buf = root.map(Path::to_path_buf).unwrap_or_else(lhc_root);
        let root = root_buf.as_path();
        if let Err(err) = std::fs::create_dir_all(root.join("threads")) {
            error!(?err, "LHC: failed to create threads directory");
            return None;
        }

        let registry_path = root.join("registry.sqlite");
        let file_path = thread_file_path(root, thread_id);

        let lhc = init_lhc(SdkConfig {
            inference_callbacks: Some(inference_callbacks),
            inference: None,
            mode,
            clock: None,
            guards: None,
            tool_result: None,
            lease: None,
            chunk_policy: None,
            view: None,
        });

        let registry_str = registry_path.to_string_lossy().into_owned();
        let thread_ref = if file_path.exists() {
            open_existing(&lhc, thread_id, &file_path, &registry_str).await?
        } else {
            create_new(&lhc, thread_id, cwd, &file_path, &registry_str).await?
        };

        let mut session = Self {
            thread_id: thread_id.to_string(),
            lhc,
            thread_ref,
            file_path,
            registry_path,
            generation: 0,
            capture_disabled: false,
            failure_count: 0,
        };

        let tracker = match session.seed_from_db().await {
            Ok(v) => v,
            Err(err) => {
                error!(
                    thread_id,
                    %err,
                    "LHC: list_events failed at open; refusing"
                );
                return None;
            }
        };

        Some((session, tracker))
    }

    /// Seed tip and occurrence tracker from stored events.
    pub async fn seed_from_db(&mut self) -> Result<OccurrenceTracker, String> {
        let events = self.list_events().await?;
        self.generation = events
            .iter()
            .map(EventRecord::event_order)
            .max()
            .unwrap_or(0)
            .max(0) as u64;
        let keys: Vec<&str> = events.iter().map(EventRecord::idempotency_key).collect();
        Ok(seed_occurrence_from_keys(keys))
    }

    pub fn latch_generation_from_batch(&mut self, batch: &lhc::intake_stream::BatchResult) {
        let tip = batch.thread_position.last_event_order.max(0) as u64;
        self.generation = self.generation.max(tip);
    }

    pub async fn submit_events(
        &mut self,
        events: &[lhc::intake_stream::MessageEventInput],
    ) -> Result<lhc::intake_stream::BatchResult, String> {
        if self.capture_disabled {
            return Err("capture disabled for session".into());
        }
        if events.is_empty() {
            return Ok(lhc::intake_stream::BatchResult {
                events: vec![],
                turn_transitions: vec![],
                queued_work: vec![],
                thread_position: lhc::intake_stream::ThreadPosition {
                    last_event_order: self.generation as i64,
                },
            });
        }
        let result = self
            .lhc
            .intake_stream
            .message_events(self.thread_ref.clone(), events)
            .await;
        match result {
            OpResult::Ok { value } => {
                self.failure_count = 0;
                self.latch_generation_from_batch(&value);
                Ok(value)
            }
            OpResult::Err { error } => {
                self.failure_count = self.failure_count.saturating_add(1);
                let msg = format!(
                    "LHC message_events failed (class={:?} code={:?}): {}",
                    error.error_class, error.code, error.reason
                );
                error!(thread_id = %self.thread_id, %msg);
                if self.failure_count >= 3 {
                    warn!(
                        thread_id = %self.thread_id,
                        "LHC: disabling further capture after repeated failures"
                    );
                    self.capture_disabled = true;
                }
                Err(msg)
            }
        }
    }

    pub async fn list_events(&self) -> Result<Vec<lhc::intake_stream::EventRecord>, String> {
        match self
            .lhc
            .intake_stream
            .list_events(self.thread_ref.clone())
            .await
        {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub async fn list_turns(&self) -> Result<Vec<lhc::turns::TurnRecord>, String> {
        match self.lhc.turns.list_turns(self.thread_ref.clone()).await {
            OpResult::Ok { value } => Ok(value),
            OpResult::Err { error } => Err(error.reason),
        }
    }

    /// Wait for this session's scheduler to report the thread quiescent.
    /// Inert (returns immediately) on a `Manual` session — only the capture
    /// session has a scheduler.
    pub async fn drain_settled(&self) {
        self.lhc.drain_settled(self.thread_ref.clone()).await;
    }

    /// Bound on [`Self::close`]'s settle-wait. Close is cleanup, not a gate:
    /// give in-flight background derivation a moment to land, then let go.
    const CLOSE_SETTLE_BOUND: Duration = Duration::from_secs(5);

    pub async fn close(self) {
        // Best-effort settle before the session (and, on the capture worker,
        // its runtime) drops. Bounded: a scheduler that cannot settle — a
        // hung inference call, callbacks that never resolve — must not turn
        // close into a hang. Whatever is still in flight stays claimed in the
        // durable work queue; its lease expires and first-touch catch-up
        // re-drains it on the next open. On a `Manual` session the scheduler
        // holds no state and this returns immediately.
        let _ = tokio::time::timeout(
            Self::CLOSE_SETTLE_BOUND,
            self.lhc.drain_settled(self.thread_ref.clone()),
        )
        .await;
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn poison(&mut self) {
        self.capture_disabled = false;
        self.failure_count = 2;
        self.thread_ref = ThreadRef::file_path("/nonexistent/lhc-poisoned.sqlite");
    }
}

/// Filesystem-safe path component for a thread id.
/// Filesystem-safe path component. Uses percent-encoding so `a:b` and `a_b`
/// never collide (F10).
pub fn encode_thread_id_for_path(thread_id: &str) -> String {
    let mut out = String::with_capacity(thread_id.len() * 2);
    for b in thread_id.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' => out.push(b as char),
            _ => {
                out.push('%');
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push(HEX[usize::from(b >> 4)] as char);
                out.push(HEX[usize::from(b & 0xf)] as char);
            }
        }
    }
    out
}

pub fn thread_file_path(root: &Path, thread_id: &str) -> PathBuf {
    root.join("threads")
        .join(format!("{}.sqlite", encode_thread_id_for_path(thread_id)))
}

async fn open_existing(
    lhc: &Lhc,
    thread_id: &str,
    file_path: &Path,
    registry_str: &str,
) -> Option<ThreadRef> {
    let path_str = file_path.to_string_lossy().into_owned();
    let info = match lhc
        .threads
        .info(ThreadRef::file_path(path_str.clone()))
        .await
    {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => {
            error!(
                thread_id,
                path = %file_path.display(),
                reason = %error.reason,
                "LHC: thread file exists but info() failed; refusing"
            );
            return None;
        }
    };
    match lhc
        .threads
        .resolve(ResolveInput {
            thread_id: info.thread_id.clone(),
            registry_path: Some(registry_str.to_string()),
        })
        .await
    {
        OpResult::Ok { value: _ } => Some(ThreadRef::file_path(path_str)),
        OpResult::Err { error } => {
            error!(
                thread_id,
                reason = %error.reason,
                "LHC: resolve failed for existing thread; refusing"
            );
            None
        }
    }
}

/// Serialize registry CREATE TABLE races. Intentional MutexGuard hold across
/// the SDK await — concurrent `new_thread` on the same registry is unsafe.
#[allow(clippy::await_holding_invalid_type)]
async fn create_new(
    lhc: &Lhc,
    thread_id: &str,
    cwd: Option<&str>,
    file_path: &Path,
    registry_str: &str,
) -> Option<ThreadRef> {
    let path_str = file_path.to_string_lossy().into_owned();
    let _guard = registry_lock().lock().await;
    match lhc
        .threads
        .new_thread(NewThreadInput {
            file_path: path_str.clone(),
            title: Some(format!("codex:{thread_id}")),
            cwd: cwd.map(str::to_string),
            registry_path: Some(registry_str.to_string()),
        })
        .await
    {
        OpResult::Ok { value: _ } => Some(ThreadRef::file_path(path_str)),
        OpResult::Err { error } => {
            error!(
                thread_id,
                reason = %error.reason,
                "LHC: new_thread failed"
            );
            None
        }
    }
}
