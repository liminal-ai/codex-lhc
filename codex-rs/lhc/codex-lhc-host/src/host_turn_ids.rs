//! Map LHC synthetic turn labels (`t{n}`) back to host UUID turn ids.
//!
//! Compact rewrite used to emit `TurnStarted` / `TurnAborted` with the SDK's
//! `t{order}` names. Upstream `thread_history` keys turns by the host UUID
//! (`Event.id`). Resume then drops turn-scoped items and can emit a second
//! interrupt banner (F2).
//!
//! Mapping prefers unknown over a wrong host id: an inert `turn_end` maps
//! nothing, and the prior-generation suffix is applied only when remaining
//! unmatched turns and remaining unused host starts correspond one-to-one.

use std::collections::HashMap;
use std::collections::HashSet;

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
/// 1. `turn_end` keys whose `event_order` equals a turn's `closed_at_event_order`.
///    An inert `turn_end` (closed no turn) maps nothing.
/// 2. The live open turn at rewrite, labeled from the compact arm's current
///    host turn id (and capture binding when present), not from event inference.
/// 3. Remaining synthetic turns zip remaining unused prior `TurnStarted` UUIDs
///    only when those two sequences are the same length.
pub fn host_turn_id_map(
    turns: &[TurnRecord],
    events: &[EventRecord],
    prior: &[RolloutItem],
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

    label_live_open_turn(
        turns,
        current_host_turn_id,
        current_lhc_turn_id,
        &mut map,
        &mut consumed,
    );

    let remaining_prior: Vec<String> = prior_host_turn_started_ids(prior)
        .into_iter()
        .filter(|host| !consumed.contains(host))
        .collect();
    let mut unmatched: Vec<&TurnRecord> = turns
        .iter()
        .filter(|turn| is_synthetic_lhc_turn_id(&turn.turn_id) && !map.contains_key(&turn.turn_id))
        .collect();
    unmatched.sort_by_key(|turn| turn.turn_order);
    if !unmatched.is_empty() && unmatched.len() == remaining_prior.len() {
        for (turn, host) in unmatched.iter().zip(remaining_prior.iter()) {
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

fn label_live_open_turn(
    turns: &[TurnRecord],
    current_host_turn_id: Option<&str>,
    current_lhc_turn_id: Option<&str>,
    map: &mut HashMap<String, String>,
    consumed: &mut HashSet<String>,
) {
    let Some(host) = current_host_turn_id else {
        return;
    };
    if consumed.contains(host) {
        return;
    }
    if let Some(lhc) = current_lhc_turn_id {
        if map.contains_key(lhc) {
            return;
        }
        if turns.iter().any(|turn| turn.turn_id == lhc) {
            map.insert(lhc.to_string(), host.to_string());
            consumed.insert(host.to_string());
        }
        return;
    }
    let opens: Vec<&TurnRecord> = turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::Open && !map.contains_key(&turn.turn_id))
        .collect();
    if let [turn] = opens.as_slice() {
        map.insert(turn.turn_id.clone(), host.to_string());
        consumed.insert(host.to_string());
    }
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

#[cfg(test)]
#[path = "host_turn_ids_tests.rs"]
mod tests;
