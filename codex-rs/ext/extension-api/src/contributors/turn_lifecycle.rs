use codex_protocol::config_types::CollaborationMode;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;

use crate::ExtensionData;

/// Input supplied when the host starts a turn.
pub struct TurnStartInput<'a> {
    /// Stable host-owned turn identifier.
    pub turn_id: &'a str,
    /// Effective collaboration mode for this turn.
    pub collaboration_mode: &'a CollaborationMode,
    /// Total token usage snapshot captured when the turn started.
    pub token_usage_at_turn_start: &'a TokenUsage,
    /// Host-reported turn start time (Unix seconds), when known.
    ///
    /// LHC-HOOK: optional host wall-clock start for turn_end payload (schema v5).
    pub started_at: Option<i64>,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}

/// Input supplied when the host completes a turn.
pub struct TurnStopInput<'a> {
    /// Host-reported turn start time (Unix seconds), when known.
    ///
    /// LHC-HOOK: optional host wall-clock start for turn_end payload (schema v5).
    pub started_at: Option<i64>,
    /// Host-reported turn end time (Unix seconds), when known.
    ///
    /// LHC-HOOK: optional host wall-clock end for turn_end payload (schema v5).
    pub completed_at: Option<i64>,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}

/// Input supplied when the host aborts a turn.
pub struct TurnAbortInput<'a> {
    /// Reason the host aborted the turn.
    pub reason: TurnAbortReason,
    /// Host-reported turn start time (Unix seconds), when known.
    ///
    /// LHC-HOOK: optional host wall-clock start for turn_end payload (schema v5).
    pub started_at: Option<i64>,
    /// Host-reported turn abort time (Unix seconds), when known.
    ///
    /// LHC-HOOK: optional host wall-clock end for turn_end payload (schema v5).
    pub completed_at: Option<i64>,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}

/// Input supplied when the host observes an error for a turn.
pub struct TurnErrorInput<'a> {
    /// Stable host-owned turn identifier.
    pub turn_id: &'a str,
    /// Error surfaced by the host for this turn.
    pub error: CodexErrorInfo,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}
