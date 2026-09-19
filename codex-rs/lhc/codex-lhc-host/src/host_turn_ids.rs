//! Map LHC synthetic turn labels (`t{n}`) back to host UUID turn ids.
//!
//! Compact rewrite used to emit `TurnStarted` / `TurnAborted` with the SDK's
//! `t{order}` names. Upstream `thread_history` keys turns by the host UUID
//! (`Event.id`). Resume then drops turn-scoped items and can emit a second
//! interrupt banner (F2).
//!
//! Proven binding or correspondence, otherwise unknown: closing `turn_end`
//! keys, and the compact arm's capture-bound live turn. No prior-generation
//! zip, no sole-open inference.

use std::collections::HashMap;
use std::collections::HashSet;

use lhc::intake_stream::EventRecord;
use lhc::turns::TurnRecord;

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
/// 1. `turn_end` keys whose `event_order` equals a turn's `closed_at_event_order`.
///    An inert `turn_end` (closed no turn) maps nothing.
/// 2. The live open turn, only when `current_lhc_turn_id` is the capture binding
///    and that id is present in `turns`.
pub fn host_turn_id_map(
    turns: &[TurnRecord],
    events: &[EventRecord],
    current_host_turn_id: Option<&str>,
    current_lhc_turn_id: Option<&str>,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut consumed: HashSet<String> = HashSet::new();

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
        let Some(turn) = turns
            .iter()
            .find(|turn| turn.closed_at_event_order == Some(*event_order))
        else {
            continue;
        };
        if consumed.contains(&host) || map.contains_key(&turn.turn_id) {
            continue;
        }
        map.insert(turn.turn_id.clone(), host.clone());
        consumed.insert(host);
    }

    if let (Some(host), Some(lhc)) = (current_host_turn_id, current_lhc_turn_id)
        && !consumed.contains(host)
        && !map.contains_key(lhc)
        && turns.iter().any(|turn| turn.turn_id == lhc)
    {
        map.insert(lhc.to_string(), host.to_string());
    }

    map
}

/// Emit this id on `TurnStarted` / `TurnAborted` / `TurnComplete`.
pub fn display_turn_id(lhc_turn_id: &str, map: &HashMap<String, String>) -> String {
    map.get(lhc_turn_id)
        .cloned()
        .unwrap_or_else(|| lhc_turn_id.to_string())
}

#[cfg(test)]
#[path = "host_turn_ids_tests.rs"]
mod tests;
