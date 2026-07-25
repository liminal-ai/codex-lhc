use codex_protocol::models::ResponseItem;

use crate::ExtensionData;
use crate::ExtensionFuture;

/// Host-supplied origin of a raw conversation item batch.
///
/// Discriminator for capture classification. Set at the recording call site —
/// never inferred from rendered text (FORK.md law 6). Exhaustive match required
/// at every consumer; no wildcard arm.
///
/// LHC-HOOK: additive enum, designed to be upstreamable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RawItemProvenance {
    /// Real human input (`record_user_prompt_and_emit_turn_item`).
    UserPrompt,
    /// Model/tool/stream output (`record_response_item_and_emit_turn_item`,
    /// stream events, tool results).
    ModelOutput,
    /// Host scaffolding: initial context, time reminders, budgets, hooks,
    /// developer messages, world-state diffs.
    HostContext,
    /// Inter-agent traffic (`record_inter_agent_communication`).
    InterAgent,
}

/// Input supplied when the host records raw conversation items.
///
/// Items are the pre-normalization `ResponseItem` values the host is about to
/// fan out as `EventMsg::RawResponseItem`. Extensions that need full fidelity
/// (encrypted reasoning, exact function-call JSON, call_id linkage) should
/// observe this path rather than prompt-fragment or turn-item hooks.
///
/// LHC-HOOK: additive only — designed to be upstreamable. See FORK.md.
pub struct RawItemInput<'a> {
    /// Raw response items being recorded for this thread.
    pub items: &'a [ResponseItem],
    /// Typed origin of this batch (call-site supplied).
    pub provenance: RawItemProvenance,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime (`level_id` is the thread id).
    pub thread_store: &'a ExtensionData,
    /// Store scoped to the current turn, when a turn is active.
    pub turn_store: Option<&'a ExtensionData>,
}

/// Contributor for host-owned raw conversation item recording.
///
/// Implementations must stay cheap on the session path: do not block on I/O.
/// Heavy work belongs on a background drain (for example `on_thread_idle`).
/// Panic/timeout containment is the adapter's responsibility.
pub trait RawItemContributor: Send + Sync {
    /// Called after the host records items into history and is about to emit
    /// raw-response-item events.
    fn on_raw_items<'a>(&'a self, input: RawItemInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _self = self;
            let _input = input;
        })
    }
}
