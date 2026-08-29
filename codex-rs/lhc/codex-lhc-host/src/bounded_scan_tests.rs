//! LIM-135: bounded capture open, indexed coverage, and marker-key fallbacks.

use std::collections::HashSet;
use std::path::Path;

use codex_extension_api::RawItemProvenance;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use lhc::intake_stream::MessageEventInput;
use lhc::sdk::OpResult;
use lhc::shared_tech::storage::SqlParam;
use lhc::threads::open_thread_database;
use pretty_assertions::assert_eq;
use serde_json::Map;
use serde_json::json;
use tempfile::tempdir;

use crate::DerivedProvenance;
use crate::LhcSession;
use crate::compact_bridge::CompactMarker;
use crate::compact_bridge::VIEW_MAP_SEAM_ID;
use crate::compact_bridge::content_identity_digest;
use crate::compact_bridge::derived_from_session_and_archive_bounded;
use crate::compact_bridge::host_items_missing_from_archive_with_provenance;
use crate::encode_thread_id;
use crate::inference::lhc_inference_callbacks;
use crate::item_stable_id;
use crate::projections::compact_marker_key_prefix;
use crate::projections::compact_point_from_marker_key;
use crate::projections::host_items_missing_from_archive_indexed;
use crate::projections::list_keys_under_prefix;
use crate::rollout_reconcile::latest_compact_point_from_events;
use crate::session::thread_file_path;
use crate::spawn_capture;

fn user(text: &str, id: Option<&str>) -> ResponseItem {
    ResponseItem::Message {
        id: id.map(|id| ResponseItemId::from_server(id.into())),
        role: "user".into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn open_session(root: &Path, tid: &str) -> (LhcSession, crate::OccurrenceTracker) {
    LhcSession::open_with_inference(
        tid,
        None,
        Some(root),
        lhc_inference_callbacks(false).expect("deterministic"),
    )
    .await
    .expect("open")
}

async fn persist_items(root: &Path, tid: &str, items: &[ResponseItem]) {
    let derivation = crate::inference::LateBoundCallbacks::new();
    derivation.seed(lhc_inference_callbacks(false).unwrap());
    let handle = spawn_capture(tid, None, Some(root.to_path_buf()), derivation)
        .await
        .expect("capture");
    for item in items {
        handle.persist(
            item,
            RawItemProvenance::UserPrompt,
            /*step_index*/ None,
        );
    }
    handle.flush().await;
    handle.shutdown().await;
}

fn insert_unparsable_unused_payload(root: &Path, tid: &str) {
    let path = thread_file_path(root, tid);
    let db = match open_thread_database(&path.to_string_lossy()) {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => panic!("open_thread_database: {}", error.reason),
    };
    let max = db
        .prepare("SELECT MAX(event_order) AS m FROM event")
        .get()
        .and_then(|row| row.get("m").and_then(serde_json::Value::as_i64))
        .unwrap_or(0);
    let order = max + 1;
    db.prepare(
        "INSERT INTO event (event_order, event_kind, idempotency_key, actor, harness, payload, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .run(&[
        SqlParam::from(order),
        SqlParam::from("runtime_note"),
        SqlParam::from(format!("codex:{tid}:unused:garbage-payload")),
        SqlParam::from("system"),
        SqlParam::from("codex"),
        SqlParam::from("this-is-not-json"),
        SqlParam::from("2026-01-01T00:00:00.000Z"),
    ]);
    db.close();
}

async fn submit_runtime_note(root: &Path, tid: &str, key: &str, text: &str) {
    let (mut session, _) = open_session(root, tid).await;
    let mut payload = Map::new();
    payload.insert("text".into(), json!(text));
    session
        .submit_events(&[MessageEventInput {
            event_kind: "runtime_note".into(),
            idempotency_key: Some(key.to_string()),
            actor: "system".into(),
            harness: "codex".into(),
            payload,
            extra: Map::new(),
        }])
        .await
        .expect("runtime note");
    session.close().await;
}

#[test]
fn compact_point_parses_from_marker_key_without_payload() {
    assert_eq!(
        compact_point_from_marker_key("codex:tid:compact_marker:tip:12:0:fp"),
        Some(12)
    );
    assert_eq!(
        compact_point_from_marker_key("codex:t%3Ax:compact_marker:order%3A9:4:1:body"),
        Some(4)
    );
    assert_eq!(
        compact_point_from_marker_key("codex:tid:id:msg:deadbeef:user_prompt"),
        None
    );
}

#[tokio::test]
async fn capture_open_uses_constant_row_frontier_not_list_events() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let tid = "open-frontier";
    persist_items(
        root,
        tid,
        &[user("one", Some("u1")), user("two", Some("u2"))],
    )
    .await;

    let (session, tracker) = open_session(root, tid).await;
    let stats = session.query_stats();
    assert_eq!(stats.list_events_calls, 0);
    assert_eq!(stats.list_events_rows, 0);
    assert_eq!(stats.payload_parses, 0);
    assert_eq!(stats.frontier_calls, 1);
    assert_eq!(stats.frontier_rows, 1);
    assert_eq!(stats.key_list_calls, 0);
    assert!(session.generation >= 2);
    // Empty tracker: ID-bearing items do not scan anonymous history at open.
    let mut tracker = tracker;
    assert_eq!(tracker.next("any"), 0);
    session.close().await;
}

#[tokio::test]
async fn open_query_counters_do_not_grow_with_thread_history() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let small: Vec<_> = (0..2)
        .map(|i| user(&format!("s{i}"), Some(&format!("s{i}"))))
        .collect();
    persist_items(root, "small-hist", &small).await;
    let (small_session, _) = open_session(root, "small-hist").await;
    let small_stats = small_session.query_stats();
    small_session.close().await;

    let large: Vec<_> = (0..40)
        .map(|i| user(&format!("l{i}"), Some(&format!("l{i}"))))
        .collect();
    persist_items(root, "large-hist", &large).await;
    let (large_session, _) = open_session(root, "large-hist").await;
    let large_stats = large_session.query_stats();
    assert!(large_session.generation >= 40);
    large_session.close().await;

    assert_eq!(small_stats.list_events_calls, 0);
    assert_eq!(large_stats.list_events_calls, 0);
    assert_eq!(small_stats.payload_parses, 0);
    assert_eq!(large_stats.payload_parses, 0);
    assert_eq!(small_stats.frontier_calls, large_stats.frontier_calls);
    assert_eq!(small_stats.frontier_rows, large_stats.frontier_rows);
    assert_eq!(small_stats.frontier_rows, 1);
    assert_eq!(
        small_stats.list_events_rows, large_stats.list_events_rows,
        "open must not return a row set that grows with history"
    );
}

#[tokio::test]
async fn large_thread_open_does_not_parse_unused_unparsable_payload() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let tid = "unparsable-unused";
    let items: Vec<_> = (0..24)
        .map(|i| user(&format!("keep {i}"), Some(&format!("k{i}"))))
        .collect();
    persist_items(root, tid, &items).await;
    insert_unparsable_unused_payload(root, tid);

    let (session, _) = open_session(root, tid).await;
    let stats = session.query_stats();
    assert_eq!(stats.list_events_calls, 0);
    assert_eq!(stats.payload_parses, 0);
    let listed = session.list_events().await;
    assert!(
        listed.is_err(),
        "fixture must actually be unparsable when list_events runs: {listed:?}"
    );
    session.close().await;
}

#[tokio::test]
async fn indexed_coverage_matches_scan_oracle_on_stable_and_anon_items() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let tid = "coverage-parity";
    let stored = vec![
        user("present", Some("id-present")),
        user("anon-one", None),
        user("anon-one", None),
    ];
    persist_items(root, tid, &stored).await;

    let host = vec![
        user("present", Some("id-present")),
        user("ghost", Some("id-ghost")),
        user("anon-one", None),
        user("anon-one", None),
        user("anon-two", None),
    ];

    let (session, _) = open_session(root, tid).await;
    let events = session.list_events().await.expect("oracle list");
    let derived = DerivedProvenance::default();
    let scan = host_items_missing_from_archive_with_provenance(&host, &events, &derived);
    let indexed = host_items_missing_from_archive_indexed(&session, &host, &derived)
        .await
        .expect("indexed");
    assert_eq!(scan, indexed);
    assert_eq!(indexed.len(), 2, "ghost id + extra anon digest");

    let session_derived = DerivedProvenance {
        ids: HashSet::from(["id-ghost".to_string()]),
        digests: HashSet::new(),
    };
    let scan_derived =
        host_items_missing_from_archive_with_provenance(&host, &events, &session_derived);
    let indexed_derived =
        host_items_missing_from_archive_indexed(&session, &host, &session_derived)
            .await
            .expect("indexed session-derived");
    assert_eq!(scan_derived, indexed_derived);
    assert_eq!(
        indexed_derived.len(),
        1,
        "session-derived id excludes ghost; extra anon digest remains missing"
    );

    let stats = session.query_stats();
    assert!(stats.prefix_count_calls >= 1);
    assert!(stats.prefix_count_rows <= stats.prefix_count_input_prefixes);
    assert!(
        stats.prefix_count_input_prefixes <= host.len() as u64 * 2,
        "prefix queries are caller-bounded by host items, not archive length ({})",
        events.len()
    );
    session.close().await;
}

#[tokio::test]
async fn capture_frontier_is_constant_row_and_matches_last_event_order() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let tid = "frontier-count";
    persist_items(
        root,
        tid,
        &[
            user("a", Some("a")),
            user("b", Some("b")),
            user("c", Some("c")),
        ],
    )
    .await;
    let frontier = crate::read_capture_frontier(tid, Some(root))
        .await
        .expect("frontier");
    assert_eq!(frontier.last_event_order, frontier.event_count as i64);
    assert!(frontier.last_event_order >= 3);
}

#[tokio::test]
async fn compact_marker_key_prefix_none_parse_is_explicit_zero() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let tid = "marker-parity-none";
    persist_items(root, tid, &[user("a", Some("a")), user("b", Some("b"))]).await;

    let (session, _) = open_session(root, tid).await;
    let events = session.list_events().await.expect("oracle list");
    let scan_point = latest_compact_point_from_events(&events);
    let keys = list_keys_under_prefix(&session, &compact_marker_key_prefix(tid))
        .await
        .expect("marker keys");
    assert!(keys.is_empty(), "fixture has no compact-marker keys");
    assert_eq!(
        scan_point,
        Some(0),
        "no parseable marker is compact_point 0"
    );
    assert_eq!(
        keys.iter()
            .filter_map(|k| compact_point_from_marker_key(&k.idempotency_key))
            .max(),
        None
    );
    session.close().await;
}

#[tokio::test]
async fn compact_marker_key_prefix_matches_payload_scan_oracle() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let tid = "marker-parity-parseable";
    persist_items(
        root,
        tid,
        &[user("turn one", Some("u1")), user("turn two", Some("u2"))],
    )
    .await;
    let key = format!("{}tip:12:0:fp", compact_marker_key_prefix(tid));
    submit_runtime_note(
        root,
        tid,
        &key,
        &format!(r#"lhc_compact_marker {{"compactPoint":12,"viewId":"v"}}"#),
    )
    .await;

    let (session, _) = open_session(root, tid).await;
    let events = session.list_events().await.expect("oracle list");
    let scan_point = latest_compact_point_from_events(&events);
    let keys = list_keys_under_prefix(&session, &compact_marker_key_prefix(tid))
        .await
        .expect("marker keys");
    let key_point = keys
        .iter()
        .filter_map(|k| compact_point_from_marker_key(&k.idempotency_key))
        .max();
    assert_eq!(
        scan_point,
        Some(12),
        "payload scan must see compactPoint 12"
    );
    assert_eq!(
        key_point,
        Some(12),
        "marker key must encode compact_point 12"
    );
    assert_eq!(scan_point, key_point);
    assert!(
        session.query_stats().key_list_rows < events.len() as u64,
        "marker-key rows must not grow with total history"
    );
    session.close().await;
}

#[tokio::test]
async fn indexed_coverage_matches_scan_oracle_on_legacy_full_note_markers() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let tid = "coverage-legacy-marker";
    let stored = vec![user("source turn", Some("id-src"))];
    persist_items(root, tid, &stored).await;

    let derived_item = user("installed body", Some("installed-1"));
    let marker = CompactMarker {
        view_id: "legacy".into(),
        covered_from: 0,
        compact_point: 3,
        total_tokens: 0,
        tail_tokens: 0,
        first_kept_message_id: None,
        profile: None,
        bands: serde_json::Value::Null,
        view_map_seam: VIEW_MAP_SEAM_ID.into(),
        body_item_count: 1,
        marker_key: format!("codex:{}:compact_marker:tip:3:0:fp", encode_thread_id(tid)),
        derived_content_digests: vec![content_identity_digest(&derived_item)],
        derived_host_ids: vec!["installed-1".into()],
        archive_tip: "tip".into(),
    };
    let full_note = format!(
        "lhc_compact_marker {}",
        serde_json::to_string(&marker).expect("marker json")
    );
    submit_runtime_note(root, tid, &marker.marker_key, &full_note).await;

    let host = vec![
        user("source turn", Some("id-src")),
        user("installed body", Some("installed-1")),
        user("ghost", Some("id-ghost")),
        user("installed body", None),
        user("anon-extra", None),
    ];

    let (session, _) = open_session(root, tid).await;
    let events = session.list_events().await.expect("oracle list");
    let empty = DerivedProvenance::default();
    let oracle = DerivedProvenance::from_session_and_archive(&empty.ids, &empty.digests, &events);
    assert!(
        oracle.ids.contains("installed-1"),
        "scan oracle must recover the legacy marker's derived id"
    );

    let naive = host_items_missing_from_archive_indexed(&session, &host, &empty)
        .await
        .expect("naive indexed");
    assert!(
        naive
            .iter()
            .any(|i| item_stable_id(i).as_deref() == Some("installed-1")),
        "pre-restoration (session-empty, no archive-marker walk) would re-import the derived id"
    );

    let recovered = derived_from_session_and_archive_bounded(&session, &empty)
        .await
        .expect("bounded archive recovery");
    assert_eq!(oracle.ids, recovered.ids);
    assert_eq!(oracle.digests, recovered.digests);

    let scan = host_items_missing_from_archive_with_provenance(&host, &events, &oracle);
    let indexed = host_items_missing_from_archive_indexed(&session, &host, &recovered)
        .await
        .expect("indexed recovered");
    assert_eq!(scan, indexed);
    assert!(
        indexed
            .iter()
            .all(|i| item_stable_id(i).as_deref() != Some("installed-1")),
        "recovered provenance must exclude the derived id"
    );
    session.close().await;
}
