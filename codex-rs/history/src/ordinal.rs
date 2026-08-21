use std::io;

use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::ThreadHistoryMode;

/// Canonical ordinal assignment state for persisted rollout records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RolloutOrdinalState {
    /// Legacy rollout records do not carry top-level ordinals.
    Legacy,
    /// Paginated rollout records carry contiguous top-level ordinals.
    Paginated { next: Option<u64> },
}

impl RolloutOrdinalState {
    /// Start a newly created rollout at its history-base boundary.
    pub fn for_new_rollout(
        history_mode: ThreadHistoryMode,
        history_base: Option<HistoryPosition>,
    ) -> Self {
        match history_mode {
            ThreadHistoryMode::Legacy => Self::Legacy,
            ThreadHistoryMode::Paginated => Self::Paginated {
                next: Some(history_base.map_or(0, |base| base.end_ordinal_exclusive)),
            },
        }
    }

    /// Start a full rewrite while preserving paginated history-base and subagent boundaries.
    pub fn for_rewrite(
        history_mode: ThreadHistoryMode,
        history_base: Option<HistoryPosition>,
        subagent_history_start_ordinal: Option<u64>,
        item_count: usize,
    ) -> io::Result<Self> {
        if history_mode == ThreadHistoryMode::Legacy {
            return Ok(Self::Legacy);
        }
        let item_count = u64::try_from(item_count)
            .map_err(|_| io::Error::other("paginated rollout rewrite is too large"))?;
        let history_start = history_base.map_or(0, |base| base.end_ordinal_exclusive);
        let minimum_next = history_start
            .checked_add(item_count)
            .ok_or_else(|| io::Error::other("paginated rollout record ordinal overflow"))?;
        let next_after_rewrite = subagent_history_start_ordinal
            .map_or(minimum_next, |subagent_start| {
                subagent_start.max(minimum_next)
            });
        let first = next_after_rewrite
            .checked_sub(item_count)
            .ok_or_else(|| io::Error::other("paginated rollout record ordinal underflow"))?;
        Ok(Self::Paginated { next: Some(first) })
    }

    /// Return the ordinal for the next record, if this is a paginated rollout.
    pub fn current(&self) -> io::Result<Option<u64>> {
        match self {
            Self::Legacy => Ok(None),
            Self::Paginated { next } => {
                let ordinal = (*next)
                    .ok_or_else(|| io::Error::other("paginated rollout record ordinal overflow"))?;
                Ok(Some(ordinal))
            }
        }
    }

    /// Advance after one record has been durably written.
    pub fn advance(&mut self) {
        if let Self::Paginated { next } = self
            && let Some(ordinal) = *next
        {
            *next = ordinal.checked_add(1);
        }
    }
}
