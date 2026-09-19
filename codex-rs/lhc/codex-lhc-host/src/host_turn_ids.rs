//! Map LHC synthetic turn labels (`t{n}`) back to host UUID turn ids.
//!
//! Compact rewrite used to emit `TurnStarted` / `TurnAborted` with the SDK's
//! `t{order}` names. Upstream `thread_history` keys turns by the host UUID
//! (`Event.id`). Resume then drops turn-scoped items and can emit a second
//! interrupt banner (F2).

use std::collections::HashMap;

use codex_history::RolloutItem;
use codex_protocol::protocol::EventMsg;
use lhc::intake_stream::EventRecord;
use lhc::turns::TurnRecord;
use lhc::turns::TurnStatus;

/// True for the SDK's `t{order}` labels (`t1`, `t11`). Host UUIDs never match.
pub fn is_synthetic_lhc_turn_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix('t') else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Inverse of [`crate::idempotency::turn_end_key`].
///
/// `codex:{thread}:turn_end:{turn_id}:{reason}` — `turn_id` is the host UUID
/// passed to capture (`LhcTurnId` / `Event.id`), not the SDK `t{n}` label.
pub fn parse_host_turn_id_from_turn_end_key(key: &str) -> Option<String> {
    let rest = key.strip_prefix("codex:")?;
    let mut parts = rest.split(':');
    let _thread = parts.next()?;
    if parts.next()? != "turn_end" {
        return None;
    }
    let turn = parts.next()?;
    let reason = parts.next()?;
    if reason.is_empty() || parts.next().is_some() {
        return None;
    }
    // Inverse of `encode_thread_id` (`%` then `:`). Host UUIDs are unchanged.
    let decoded = turn.replace("%3A", ":").replace("%25", "%");
    if decoded.is_empty() || is_synthetic_lhc_turn_id(&decoded) {
        return None;
    }
    Some(decoded)
}

/// LHC `t{n}` → host UUID for rewrite emission.
///
/// Prefer `turn_end` idempotency keys (durable, abort-accurate). Fill remaining
/// synthetic labels from trailing non-synthetic `TurnStarted` ids in the prior
/// generation so a mid-turn rewrite before `turn_end` lands still preserves the
/// live UUID.
pub fn host_turn_id_map(
    turns: &[TurnRecord],
    events: &[EventRecord],
    prior: &[RolloutItem],
) -> HashMap<String, String> {
    let mut map = HashMap::new();

    for event in events {
        let EventRecord::TurnEnd {
            idempotency_key,
            event_order,
            ..
        } = event
        else {
            continue;
        };
        let Some(host) = parse_host_turn_id_from_turn_end_key(idempotency_key) else {
            continue;
        };
        if let Some(turn) = turns
            .iter()
            .find(|turn| turn.closed_at_event_order == Some(*event_order))
        {
            map.insert(turn.turn_id.clone(), host);
            continue;
        }
        if let Some(turn) = turns
            .iter()
            .filter(|turn| turn.status == TurnStatus::Open)
            .max_by_key(|turn| turn.turn_order)
            && turn.opened_at_event_order <= *event_order
        {
            map.insert(turn.turn_id.clone(), host);
        }
    }

    let prior_ids = prior_host_turn_started_ids(prior);
    let mut unmatched: Vec<&TurnRecord> = turns
        .iter()
        .filter(|turn| is_synthetic_lhc_turn_id(&turn.turn_id) && !map.contains_key(&turn.turn_id))
        .collect();
    unmatched.sort_by_key(|turn| turn.turn_order);
    if !unmatched.is_empty() && prior_ids.len() >= unmatched.len() {
        let start = prior_ids.len() - unmatched.len();
        for (turn, host) in unmatched.iter().zip(prior_ids[start..].iter()) {
            map.insert(turn.turn_id.clone(), host.clone());
        }
    }

    map
}

/// Emit this id on `TurnStarted` / `TurnAborted` / `TurnComplete`.
pub fn display_turn_id(lhc_turn_id: &str, map: &HashMap<String, String>) -> String {
    map.get(lhc_turn_id)
        .cloned()
        .unwrap_or_else(|| lhc_turn_id.to_string())
}

fn prior_host_turn_started_ids(prior: &[RolloutItem]) -> Vec<String> {
    let after_compact = prior
        .iter()
        .rposition(|item| matches!(item, RolloutItem::Compacted(_)))
        .map_or(0, |idx| idx + 1);
    prior[after_compact..]
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(ev))
                if !is_synthetic_lhc_turn_id(&ev.turn_id) =>
            {
                Some(ev.turn_id.clone())
            }
            _ => None,
        })
        .collect()
}
