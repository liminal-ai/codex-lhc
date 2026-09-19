//! Inputs from `evidence/C/F2-review-mapping-counterexamples.rs` (original)
//! and `evidence/C/F2-review-26916c1336-counterexamples.rs` (second review).
//! Expected values: proven binding or correspondence, otherwise unknown.

use super::host_turn_id_map;
use super::is_synthetic_lhc_turn_id;
use super::parse_host_turn_id_from_turn_end_key;
use lhc::intake_stream::EventRecord;
use lhc::intake_stream::TurnEndPayload;
use lhc::turns::TurnRecord;
use lhc::turns::TurnStatus;
use pretty_assertions::assert_eq;

const A: &str = "00000000-0000-0000-0000-000000000001";
const B: &str = "00000000-0000-0000-0000-000000000002";
const C: &str = "00000000-0000-0000-0000-000000000003";

fn turn(id: &str, n: i64, opened: i64, closed: Option<i64>) -> TurnRecord {
    TurnRecord {
        turn_id: id.into(),
        turn_order: n,
        status: if closed.is_some() {
            TurnStatus::Closed
        } else {
            TurnStatus::Open
        },
        member_message_ids: Vec::new(),
        opened_at_event_order: opened,
        closed_at_event_order: closed,
        outcome: None,
        outcome_reason: None,
        started_at: None,
        ended_at: None,
        chunk_id: None,
        member_idx: None,
        derivations: None,
    }
}

fn end(id: &str, n: i64) -> EventRecord {
    EventRecord::TurnEnd {
        idempotency_key: crate::turn_end_key("thread", id, "completed"),
        actor: "assistant".into(),
        harness: "codex".into(),
        payload: TurnEndPayload::default(),
        event_order: n,
        recorded_at: "2026-01-01T00:00:00.000Z".into(),
    }
}

#[test]
fn mixed_keyed_unkeyed_does_not_reuse_consumed_host_on_suffix() {
    // Original counterexample 1: end(B,5) keys t2 only. No suffix zip.
    let map = host_turn_id_map(
        &[
            turn("t1", 1, 0, Some(3)),
            turn("t2", 2, 3, Some(5)),
            turn("t3", 3, 5, None),
        ],
        &[end(B, 5)],
        None,
        None,
    );
    assert_eq!(map.get("t2").map(String::as_str), Some(B));
    assert_eq!(map.get("t1"), None);
    assert_eq!(map.get("t3"), None);
}

#[test]
fn inert_turn_end_maps_nothing_open_turn_stays_unknown() {
    // Original counterexample 2: end(A,3) closes t1; end(B,4) closed no turn.
    let map = host_turn_id_map(
        &[turn("t1", 1, 0, Some(3)), turn("t2", 2, 3, None)],
        &[end(A, 3), end(B, 4)],
        None,
        None,
    );
    assert_eq!(map.get("t1").map(String::as_str), Some(A));
    assert_eq!(map.get("t2"), None);
}

#[test]
fn live_open_turn_uses_compact_bridge_host_id_not_inert_end() {
    let map = host_turn_id_map(
        &[turn("t1", 1, 0, Some(3)), turn("t2", 2, 3, None)],
        &[end(A, 3), end(B, 4)],
        Some(C),
        Some("t2"),
    );
    assert_eq!(map.get("t1").map(String::as_str), Some(A));
    assert_eq!(map.get("t2").map(String::as_str), Some(C));
}

#[test]
fn short_prior_equal_count_does_not_assign_inert_host_to_open_turn() {
    // Second-review counterexample: prior [A,B] would have equal remaining
    // [t2]/[B] after consuming A; that is not correspondence. t2 stays unknown.
    let map = host_turn_id_map(
        &[turn("t1", 1, 0, Some(3)), turn("t2", 2, 3, None)],
        &[end(A, 3), end(B, 4)],
        None,
        None,
    );
    assert_eq!(map.get("t1").map(String::as_str), Some(A));
    assert_eq!(map.get("t2"), None);
}

#[test]
fn preturn_without_binding_does_not_label_sole_open_turn() {
    // Second-review counterexample: current host B, no LHC binding. Sole open
    // t1 is unfinished A, not B.
    let map = host_turn_id_map(&[turn("t1", 1, 0, None)], &[], Some(B), None);
    assert_eq!(map.get("t1"), None);
}

#[test]
fn turn_end_key_round_trips_host_uuid_not_synthetic_label() {
    let host = "01a0ba7a-dcef-72e0-9e69-e7ba32988a2e";
    let key = crate::turn_end_key("thread", host, "aborted");
    assert_eq!(
        parse_host_turn_id_from_turn_end_key(&key),
        Some(host.into())
    );
    assert!(is_synthetic_lhc_turn_id("t11"));
    assert!(!is_synthetic_lhc_turn_id(host));
    assert_eq!(
        parse_host_turn_id_from_turn_end_key(&crate::turn_end_key("thread", "t11", "aborted")),
        None
    );
}
