use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskResult;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default)]
pub(crate) struct CompactTask;

impl SessionTask for CompactTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Compact
    }

    fn span_name(&self) -> &'static str {
        "session_task.compact"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        // LHC-HOOK: bound, not `_cancellation_token` — the LHC arm runs
        // produce work and must stop on abort (N3).
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let _profile_guard = ctx.turn_timing_state.begin_compaction();
        // LHC-HOOK: strict LHC-only compact (no TokenBudget / remote / local).
        crate::compact_lhc::run_strict_lhc_compact(
            &session,
            ctx.as_ref(),
            crate::compact::InitialContextInjection::DoNotInject,
            /*manual*/ true,
            &cancellation_token,
        )
        .await?;
        Ok(None)
    }
}
