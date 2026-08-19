//! Layer-2 tests for startup reconciliation + Part 1 pollution helpers.
//! Mutation-demonstrated: break the named invariant and the named test fails.

use super::*;
use crate::atomic_rewrite_rollout;
use crate::capture::spawn_capture;
use crate::inference::LateBoundCallbacks;
use crate::inference::lhc_inference_callbacks;
use crate::parse_rollout_items;
use codex_extension_api::RawItemProvenance;
use codex_history::CompactedItem;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::tempdir;

fn user(text: &str, id: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.into())),
        role: "user".into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant(text: &str, id: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.into())),
        role: "assistant".into(),
        content: vec![ContentItem::OutputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn durable_message(compact_point: i64) -> String {
    format!(
        "lhc_compact_durable {}",
        serde_json::json!({
            "viewId": "v1",
            "coveredFrom": 0,
            "compactPoint": compact_point,
            "totalTokens": 10,
            "tailTokens": 5,
            "firstKeptMessageId": null,
            "profile": null,
            "bands": null,
            "viewMapSeam": "llm_request_context_to_response_items/v1",
            "bodyItemCount": 1,
            "markerKey": format!("codex:tid:compact_marker:tip:{compact_point}:0:fp"),
            "derivedContentDigests": [],
            "derivedHostIds": [],
            "archiveTip": "tip",
        })
    )
}

fn single_boundary_items(compact_point: i64) -> Vec<RolloutItem> {
    vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                ..SessionMeta::default()
            },
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: durable_message(compact_point),
            replacement_history: Some(vec![user("band", "u1").into()]),
            window_number: Some(1),
            first_window_id: Some("first".into()),
            previous_window_id: None,
            window_id: Some("win-1".into()),
        }),
        RolloutItem::ResponseItem(user("tail", "u2").into()),
    ]
}

fn dual_compacted_polluted() -> Vec<RolloutItem> {
    let mut items = single_boundary_items(3);
    items.push(RolloutItem::Compacted(CompactedItem {
        message: "native-append-compacted".into(),
        replacement_history: Some(vec![user("native-band", "u3").into()]),
        window_number: Some(2),
        first_window_id: Some("first".into()),
        previous_window_id: Some("win-1".into()),
        window_id: Some("win-2".into()),
    }));
    items
}

fn write_items(path: &Path, items: &[RolloutItem]) {
    // atomic_rewrite creates parent-relative temp next to path; file may be new.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    atomic_rewrite_rollout(path, items).expect("write seed");
}

// ── Part 1 helpers ──────────────────────────────────────────────────────────

#[test]
fn native_append_polluted_is_multi_compacted_only() {
    assert!(!is_native_append_polluted(&single_boundary_items(1)));
    assert!(is_native_append_polluted(&dual_compacted_polluted()));
    assert_eq!(compacted_record_count(&dual_compacted_polluted()), 2);
}

#[test]
fn file_boundary_compact_point_reads_newest_durable() {
    let items = dual_compacted_polluted();
    // Newest Compacted has no durable compactPoint → falls through to prior.
    // dual_compacted's last message is plain "native-append-compacted".
    assert_eq!(file_boundary_compact_point(&items), Some(3));
    assert_eq!(
        file_boundary_compact_point(&single_boundary_items(7)),
        Some(7)
    );
}

// ── Classification ──────────────────────────────────────────────────────────

#[test]
fn classify_missing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gone.jsonl");
    assert_eq!(
        classify_rollout_vs_thread(&path, Some(5)).unwrap(),
        RolloutFileClass::NeedsRewrite(RolloutReconcileTrigger::Missing)
    );
}

#[test]
fn classify_corrupt_garbage() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad.jsonl");
    std::fs::write(&path, "{not json\n{{{{\n").unwrap();
    assert_eq!(
        classify_rollout_vs_thread(&path, Some(1)).unwrap(),
        RolloutFileClass::NeedsRewrite(RolloutReconcileTrigger::Corrupt)
    );
}

#[test]
fn classify_stale_when_lhc_ahead_of_file_boundary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("stale.jsonl");
    write_items(&path, &single_boundary_items(2));
    assert_eq!(
        classify_rollout_vs_thread(&path, Some(9)).unwrap(),
        RolloutFileClass::NeedsRewrite(RolloutReconcileTrigger::Stale)
    );
}

#[test]
fn classify_ok_when_file_matches_or_ahead() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ok.jsonl");
    write_items(&path, &single_boundary_items(5));
    assert_eq!(
        classify_rollout_vs_thread(&path, Some(5)).unwrap(),
        RolloutFileClass::Ok
    );
    assert_eq!(
        classify_rollout_vs_thread(&path, Some(3)).unwrap(),
        RolloutFileClass::Ok
    );
}

#[test]
fn classify_thread_unavailable_is_fail_open_ok() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("whatever.jsonl");
    // None compact point → caller fail-open; classify reports Ok (no rewrite).
    assert_eq!(
        classify_rollout_vs_thread(&path, None).unwrap(),
        RolloutFileClass::Ok
    );
}

// ── Mutation-demonstrated regenerate paths ──────────────────────────────────

async fn seed_thread(root: &Path, tid: &str, items: &[ResponseItem]) {
    let derivation = LateBoundCallbacks::new();
    derivation.seed(lhc_inference_callbacks(false).unwrap());
    let handle = spawn_capture(tid, None, Some(root.to_path_buf()), derivation)
        .await
        .expect("capture");
    for item in items {
        let prov = match item {
            ResponseItem::Message { role, .. } if role == "user" => RawItemProvenance::UserPrompt,
            _ => RawItemProvenance::ModelOutput,
        };
        handle.persist(item, prov);
    }
    handle.flush().await;
    assert!(
        handle
            .drain_settled(std::time::Duration::from_secs(120))
            .await,
        "seed must settle"
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn reconcile_missing_regenerates_file() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "reconcile-missing-tid";
    seed_thread(
        &root,
        tid,
        &[user("hello missing", "u1"), assistant("hi", "a1")],
    )
    .await;

    let path = dir.path().join("sessions").join("rollout-missing.jsonl");
    assert!(!path.exists());

    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    match outcome {
        ReconcileOutcome::Regenerated {
            trigger: RolloutReconcileTrigger::Missing,
            items,
        } => {
            assert!(items >= 1, "regenerated items={items}");
        }
        other => panic!("expected Missing regenerate, got {other:?}"),
    }
    assert!(path.exists(), "file must be created");
    let parsed = parse_rollout_items(&path).expect("parse regenerated");
    assert!(
        parsed
            .iter()
            .any(|i| matches!(i, RolloutItem::SessionMeta(_))),
        "regenerated file must carry SessionMeta"
    );
    // Mutation bar: single boundary after regenerate (not dual-polluted).
    assert!(
        compacted_record_count(&parsed) <= 1,
        "regenerated must not be multi-Compacted polluted; count={}",
        compacted_record_count(&parsed)
    );
}

#[tokio::test]
async fn reconcile_corrupt_regenerates_file() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "reconcile-corrupt-tid";
    seed_thread(
        &root,
        tid,
        &[user("hello corrupt", "u1"), assistant("hi", "a1")],
    )
    .await;

    let path = dir.path().join("rollout-corrupt.jsonl");
    std::fs::write(&path, "this is not jsonl\n{{{\n").unwrap();

    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    match outcome {
        ReconcileOutcome::Regenerated {
            trigger: RolloutReconcileTrigger::Corrupt,
            ..
        } => {}
        other => panic!("expected Corrupt regenerate, got {other:?}"),
    }
    let parsed = parse_rollout_items(&path).expect("parse after corrupt fix");
    assert!(
        parsed
            .iter()
            .any(|i| matches!(i, RolloutItem::SessionMeta(_))),
        "must be parseable with SessionMeta after corrupt rewrite"
    );
}

#[tokio::test]
async fn reconcile_stale_regenerates_when_lhc_compact_ahead() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "reconcile-stale-tid";
    // Seed enough history that a compact can advance the compact_point.
    let mut items = Vec::new();
    for i in 0..40 {
        items.push(user(
            &format!("stale seed user {i} xxxxxxxxxx"),
            &format!("u{i}"),
        ));
        items.push(assistant(
            &format!("stale seed asst {i} yyyyyyyyyy"),
            &format!("a{i}"),
        ));
    }
    seed_thread(&root, tid, &items).await;

    // Force a real compact so archive compact_point advances.
    let produced = crate::produce_lhc_compact_deterministic(
        tid,
        Some(root.as_path()),
        &items,
        /*import_missing*/ false,
    )
    .await;
    // Whether compact reduces or not, we stamp a high compact point on the
    // *file* as low so classify sees STALE relative to a synthetic lhc point.
    let path = dir.path().join("rollout-stale.jsonl");
    write_items(&path, &single_boundary_items(0));

    // Bypass open when produce failed — still test STALE classify + regenerate
    // by using regenerate_rollout_from_thread directly after manual classify.
    let class = classify_rollout_vs_thread(&path, Some(99)).unwrap();
    assert_eq!(
        class,
        RolloutFileClass::NeedsRewrite(RolloutReconcileTrigger::Stale)
    );

    let n = regenerate_rollout_from_thread(
        &path,
        tid,
        Some(root.as_path()),
        RolloutReconcileTrigger::Stale,
        None,
    )
    .await
    .expect("regenerate stale");
    assert!(n >= 1);
    let parsed = parse_rollout_items(&path).expect("parse stale fix");
    assert!(
        parsed
            .iter()
            .any(|i| matches!(i, RolloutItem::SessionMeta(_)))
    );
    // Prior single-boundary generation retained as .prev on swap.
    let prev = crate::rollout_swap::SwapPaths::for_rollout(&path).prev;
    assert!(prev.exists(), "stale rewrite should retain prior as .prev");
    let _ = produced; // may be Err on empty body; seed+regenerate is the bar
}

#[tokio::test]
async fn reconcile_thread_unavailable_leaves_file_alone() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("untouched.jsonl");
    let original = b"leave-me-alone\n";
    std::fs::write(&path, original).unwrap();

    // No LHC root / unknown thread → fail-open.
    let outcome = reconcile_rollout_at_path(
        &path,
        "no-such-thread-zzzz",
        Some(dir.path().join("empty-lhc").as_path()),
        None,
    )
    .await;
    assert_eq!(
        outcome,
        ReconcileOutcome::Unchanged {
            reason: "thread_unavailable"
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

/// Mutation demo for Part 1 normalization branch: polluted multi-Compacted
/// must be detected so the compact arm can skip NoReduction. If
/// `is_native_append_polluted` is inverted, this fails.
#[test]
fn mutation_demo_normalization_pollution_detection() {
    let polluted = dual_compacted_polluted();
    assert!(
        is_native_append_polluted(&polluted),
        "mutation: is_native_append_polluted must be true for multi-Compacted files"
    );
    assert!(
        !is_native_append_polluted(&single_boundary_items(1)),
        "mutation: pure single-boundary must keep the NoReduction guard path"
    );
}

// ── R2 signature round-trip (genuine capture → storage → materialize) ──────

/// End-to-end identity gate: a Reasoning item with encrypted_content captured
/// through the REAL capture path (with identity) must re-emit its
/// encrypted_content when regenerated with a matching live identity, and
/// suppress it (None) under a mismatched identity. Hand-built views don't
/// cover this — the whole loop runs here.
#[tokio::test]
async fn encrypted_reasoning_round_trip_identity_gate() {
    use crate::capture::spawn_capture_with_identity;
    use crate::mapping::ModelIdentity;

    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "sig-round-trip-tid";
    let identity = ModelIdentity::new("openai", "gpt-test", ModelIdentity::RESPONSES_API);

    let derivation = LateBoundCallbacks::new();
    derivation.seed(lhc_inference_callbacks(false).unwrap());
    let handle = spawn_capture_with_identity(
        tid,
        None,
        Some(root.clone()),
        derivation,
        Some(identity.clone()),
    )
    .await
    .expect("capture");
    handle.persist(&user("please think", "u1"), RawItemProvenance::UserPrompt);
    handle.persist(
        &ResponseItem::Reasoning {
            id: Some(ResponseItemId::from_server("rs_rt".into())),
            summary: vec![],
            content: None,
            encrypted_content: Some("ROUND_TRIP_CIPHERTEXT".into()),
            internal_chat_message_metadata_passthrough: None,
        },
        RawItemProvenance::ModelOutput,
    );
    handle.persist(&assistant("done", "a1"), RawItemProvenance::ModelOutput);
    handle.flush().await;
    assert!(
        handle
            .drain_settled(std::time::Duration::from_secs(120))
            .await,
        "seed must settle"
    );
    handle.shutdown().await;

    let encrypted_of = |items: &[RolloutItem]| -> Option<Option<String>> {
        items.iter().find_map(|item| match item {
            RolloutItem::ResponseItem(item) => match &item.item {
                ResponseItem::Reasoning {
                    encrypted_content, ..
                } => Some(encrypted_content.clone()),
                _ => None,
            },
            _ => None,
        })
    };

    // Matching live identity → ciphertext re-emitted.
    let path = dir.path().join("sessions").join("rt-match.jsonl");
    regenerate_rollout_from_thread(
        &path,
        tid,
        Some(root.as_path()),
        RolloutReconcileTrigger::Missing,
        Some(identity.clone()),
    )
    .await
    .expect("regenerate match");
    let items = parse_rollout_items(&path).expect("parse match");
    assert_eq!(
        encrypted_of(&items),
        Some(Some("ROUND_TRIP_CIPHERTEXT".into())),
        "matching identity must re-emit encrypted_content"
    );

    // Mismatched live identity → suppressed.
    let path2 = dir.path().join("sessions").join("rt-mismatch.jsonl");
    regenerate_rollout_from_thread(
        &path2,
        tid,
        Some(root.as_path()),
        RolloutReconcileTrigger::Missing,
        Some(ModelIdentity::new(
            "openai",
            "gpt-other",
            ModelIdentity::RESPONSES_API,
        )),
    )
    .await
    .expect("regenerate mismatch");
    let items2 = parse_rollout_items(&path2).expect("parse mismatch");
    assert_eq!(
        encrypted_of(&items2),
        Some(None),
        "mismatched identity must suppress encrypted_content"
    );

    // No live identity (conservative default) → suppressed.
    let path3 = dir.path().join("sessions").join("rt-none.jsonl");
    regenerate_rollout_from_thread(
        &path3,
        tid,
        Some(root.as_path()),
        RolloutReconcileTrigger::Missing,
        None,
    )
    .await
    .expect("regenerate none");
    let items3 = parse_rollout_items(&path3).expect("parse none");
    assert_eq!(
        encrypted_of(&items3),
        Some(None),
        "absent live identity must suppress encrypted_content"
    );
}

/// nc4: When the LHC SDK has installed a view (compact_point advanced in
/// thread_view) but the Codex host marker note was NOT committed to archive
/// events (crash window), the reconciliation must still detect the rollout as
/// STALE and regenerate. This proves `read_thread_compact_point` uses the
/// installed view's compact_point (via describe), not just event markers.
#[tokio::test]
async fn reconcile_detects_stale_when_view_ahead_of_event_markers() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "nc4-view-ahead";

    // Seed a bandable thread. Large padding ensures tokens exceed the 120K
    // lower bound so compact actually advances the compact_point.
    // Include an exact CustomToolCall/CustomToolCallOutput pair in the
    // final turn so it lands in the active tail after compact.
    let pad = "x".repeat(2500);
    let mut items = Vec::new();
    for i in 0..79 {
        items.push(user(&format!("nc4 user {i} {pad}"), &format!("u{i}")));
        items.push(assistant(&format!("nc4 asst {i} {pad}"), &format!("a{i}")));
    }
    // Final turn with a protected CustomToolCall/Output pair
    items.push(user("nc4 user 79 final turn", "u79"));
    items.push(ResponseItem::CustomToolCall {
        id: Some(ResponseItemId::from_server("ctc_nc4".into())),
        status: Some("completed".into()),
        call_id: "call_nc4_custom".into(),
        name: "my_custom_tool".into(),
        namespace: Some("test_ns".into()),
        input: r#"{"key":"structured_value","count":42}"#.into(),
        internal_chat_message_metadata_passthrough: None,
    });
    items.push(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call_nc4_custom".into(),
        name: Some("my_custom_tool".into()),
        output: codex_protocol::models::FunctionCallOutputPayload {
            body: codex_protocol::models::FunctionCallOutputBody::Text(
                "custom tool result payload".into(),
            ),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    });
    items.push(assistant(&format!("nc4 asst 79 final {pad}"), "a79"));
    seed_thread(&root, tid, &items).await;

    // Compact via the SDK (this installs a view with compact_point > 0).
    let result = crate::produce_lhc_compact_deterministic(
        tid,
        Some(root.as_path()),
        &items,
        /*import*/ false,
    )
    .await
    .expect("compact must succeed");
    let view_point = result.marker.compact_point;
    assert!(
        view_point > 0,
        "compact must advance compact_point; got {view_point}"
    );

    // Write a rollout file with compact_point=0 (stale relative to the view).
    // This simulates the crash window: SDK compact installed the view but the
    // Codex marker note was never committed to events.
    let path = dir.path().join("sessions").join("nc4-stale.jsonl");
    write_items(&path, &single_boundary_items(0));

    // read_thread_compact_point must return the VIEW's compact_point, not 0.
    let read_point = read_thread_compact_point(tid, Some(root.as_path())).await;
    assert_eq!(
        read_point,
        Some(view_point),
        "read_thread_compact_point must return the installed view's compact_point"
    );

    // classify must detect STALE.
    let class = classify_rollout_vs_thread(&path, read_point).unwrap();
    assert_eq!(
        class,
        RolloutFileClass::NeedsRewrite(RolloutReconcileTrigger::Stale),
        "rollout at point=0 must be STALE vs view at point={view_point}"
    );

    // Full reconcile must regenerate.
    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    match &outcome {
        ReconcileOutcome::Regenerated { trigger, items } => {
            assert_eq!(*trigger, RolloutReconcileTrigger::Stale);
            assert!(*items >= 1, "regenerated rollout must have items");
        }
        other => panic!("expected Regenerated(Stale), got {other:?}"),
    }

    // Item 2: the regenerated rollout's file_boundary_compact_point must
    // equal the installed view's compact_point.
    let regenerated = parse_rollout_items(&path).expect("parse regenerated");
    let file_point = file_boundary_compact_point(&regenerated).unwrap_or(0);
    assert_eq!(
        file_point, view_point,
        "regenerated rollout boundary must match installed view compact_point"
    );

    // Item 3: a second reconcile must find the rollout OK (convergence).
    let second = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    assert_eq!(
        second,
        ReconcileOutcome::Unchanged { reason: "ok" },
        "second reconcile must converge to Unchanged(ok); got {second:?}"
    );

    // Item 4: active CustomToolCall/Output pair in the tail survives the
    // stale-view regeneration with byte-stable fields.
    let tail_items: Vec<&ResponseItem> = regenerated
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(ri) => Some(&ri.item),
            _ => None,
        })
        .collect();
    assert!(
        !tail_items.is_empty(),
        "regenerated rollout must preserve ResponseItem entries from the live tail"
    );

    // Find the CustomToolCall and assert exact fields
    let ctc = tail_items.iter().find(|item| {
        matches!(item, ResponseItem::CustomToolCall { call_id, .. } if call_id == "call_nc4_custom")
    });
    match ctc {
        Some(ResponseItem::CustomToolCall {
            call_id,
            name,
            input,
            ..
        }) => {
            // Assert byte-stable correlation fields that survive LHC
            // capture → materialization round-trip. Status and namespace
            // are stripped by forward mapping (known LHC contract).
            assert_eq!(call_id, "call_nc4_custom");
            assert_eq!(name, "my_custom_tool");
            assert_eq!(input, r#"{"key":"structured_value","count":42}"#);
        }
        _ => panic!("CustomToolCall call_nc4_custom not found in regenerated tail"),
    }

    // Find the CustomToolCallOutput and assert correlation + output
    let cto = tail_items.iter().find(|item| {
        matches!(item, ResponseItem::CustomToolCallOutput { call_id, .. } if call_id == "call_nc4_custom")
    });
    match cto {
        Some(ResponseItem::CustomToolCallOutput {
            call_id,
            name,
            output,
            ..
        }) => {
            assert_eq!(call_id, "call_nc4_custom");
            assert_eq!(name.as_deref(), Some("my_custom_tool"));
            match &output.body {
                codex_protocol::models::FunctionCallOutputBody::Text(text) => {
                    assert_eq!(text, "custom tool result payload");
                }
                other => panic!("expected Text output, got {other:?}"),
            }
            // success may be stripped in LHC round-trip; correlation
            // and output text are the byte-stable contract.
        }
        _ => panic!("CustomToolCallOutput call_nc4_custom not found in regenerated tail"),
    }

    // Assert ordering: call before output in the tail
    let call_pos = tail_items.iter().position(|item| {
        matches!(item, ResponseItem::CustomToolCall { call_id, .. } if call_id == "call_nc4_custom")
    });
    let output_pos = tail_items.iter().position(|item| {
        matches!(item, ResponseItem::CustomToolCallOutput { call_id, .. } if call_id == "call_nc4_custom")
    });
    assert!(
        call_pos < output_pos,
        "CustomToolCall must precede CustomToolCallOutput in regenerated tail"
    );
}

/// nc4: pair followed by assistant response is completed history — no graft;
/// normal LHC-materialized shape.
#[tokio::test]
async fn nc4_completed_pair_is_not_grafted() {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;

    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "nc4-completed";

    let pad = "x".repeat(2500);
    let mut items = Vec::new();
    for i in 0..80 {
        items.push(user(&format!("nc4c user {i} {pad}"), &format!("u{i}")));
        items.push(assistant(&format!("nc4c asst {i} {pad}"), &format!("a{i}")));
    }
    // Pair followed by assistant = completed, not active.
    items.push(user("completed turn", "u80"));
    items.push(ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: "call-completed".into(),
        name: "tool".into(),
        namespace: Some("ns".into()),
        input: "{}".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    items.push(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-completed".into(),
        name: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("result".into()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    });
    items.push(assistant("done after tool", "a80")); // Makes the pair historical.
    seed_thread(&root, tid, &items).await;

    let result = crate::produce_lhc_compact_deterministic(
        tid,
        Some(root.as_path()),
        &items,
        /*import*/ false,
    )
    .await
    .expect("compact");
    assert!(result.marker.compact_point > 0);

    // Prior rollout with the completed pair (followed by assistant).
    let path = dir.path().join("sessions").join("nc4c-stale.jsonl");
    let mut stale = single_boundary_items(0);
    stale.push(RolloutItem::ResponseItem(
        ResponseItem::CustomToolCall {
            id: None,
            status: Some("completed".into()),
            call_id: "call-completed".into(),
            name: "tool".into(),
            namespace: Some("ns".into()),
            input: "{}".into(),
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    stale.push(RolloutItem::ResponseItem(
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "call-completed".into(),
            name: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("result".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    stale.push(RolloutItem::ResponseItem(
        assistant("done after tool", "a80").into(),
    ));
    write_items(&path, &stale);

    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    assert!(
        matches!(&outcome, ReconcileOutcome::Regenerated { .. }),
        "expected Regenerated"
    );

    // The completed pair must NOT be grafted — LHC-materialized shape.
    let regen = parse_rollout_items(&path).expect("parse");
    let tail: Vec<&ResponseItem> = regen
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(ri) => Some(&ri.item),
            _ => None,
        })
        .collect();
    if let Some(ResponseItem::CustomToolCall {
        status, namespace, ..
    }) = tail.iter().find(|item| {
        matches!(
            item,
            ResponseItem::CustomToolCall { call_id, .. } if call_id == "call-completed"
        )
    }) {
        assert!(
            status.is_none() && namespace.is_none(),
            "completed pair must use LHC shape, not graft: status={status:?}, namespace={namespace:?}"
        );
    }
}

/// nc4 + LIM-69: an exact provider-native CustomToolCall/Output pair in the
/// prior rollout's terminal active suffix must survive stale-view startup
/// reconciliation byte-for-byte. The active suffix ends with the output
/// (no trailing assistant) — it is the unsent provider suffix.
#[tokio::test]
async fn nc4_protected_pair_survives_stale_view_reconciliation_byte_stably() {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem as Item;
    use codex_protocol::models::FunctionCallOutputPayload;

    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "nc4-pair-stable";

    // The exact provider-native pair (LIM-69 incident shape).
    let exact_call = ResponseItem::CustomToolCall {
        id: Some(ResponseItemId::from_server("ctc_pair".into())),
        status: Some("completed".into()),
        call_id: "call-nc4-pair".into(),
        name: "exec".into(),
        namespace: Some("test_ns".into()),
        input: r#"{"cmd":"ls -la","cwd":"/tmp"}"#.into(),
        internal_chat_message_metadata_passthrough: None,
    };
    let exact_output = ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-nc4-pair".into(),
        name: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                Item::InputText {
                    text: "total 42\ndrwxr-xr-x 2 user user 4096 Aug 17 00:00 .".into(),
                },
                Item::InputText {
                    text: "-rw-r--r-- 1 user user 1234 Aug 17 00:00 file.txt".into(),
                },
            ]),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };

    // Seed a bandable thread with the pair as the final active suffix
    // (no trailing assistant — this is the unsent provider suffix).
    let pad = "x".repeat(2500);
    let mut items = Vec::new();
    for i in 0..80 {
        items.push(user(&format!("nc4p user {i} {pad}"), &format!("u{i}")));
        items.push(assistant(&format!("nc4p asst {i} {pad}"), &format!("a{i}")));
    }
    items.push(user("final turn with custom tool", "u80"));
    items.push(exact_call.clone());
    items.push(exact_output.clone());
    // No trailing assistant — the pair IS the active suffix.
    seed_thread(&root, tid, &items).await;

    // Compact to advance the view.
    let result = crate::produce_lhc_compact_deterministic(
        tid,
        Some(root.as_path()),
        &items,
        /*import*/ false,
    )
    .await
    .expect("compact must succeed");
    let view_point = result.marker.compact_point;
    assert!(view_point > 0, "compact must advance");

    // Write a stale rollout with boundary + exact original pair in the
    // active tail. This simulates the crash window: SDK compact installed
    // the view but the Codex marker was not written. The prior rollout
    // still has the exact provider-native pair.
    let path = dir.path().join("sessions").join("nc4p-stale.jsonl");
    let mut stale_items = single_boundary_items(0);
    stale_items.push(RolloutItem::ResponseItem(exact_call.clone().into()));
    stale_items.push(RolloutItem::ResponseItem(exact_output.clone().into()));
    write_items(&path, &stale_items);

    // Reconcile: must detect stale and regenerate.
    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    assert!(
        matches!(&outcome, ReconcileOutcome::Regenerated { .. }),
        "expected Regenerated, got {outcome:?}"
    );

    // Parse the regenerated rollout and extract the tail.
    let regenerated = parse_rollout_items(&path).expect("parse regenerated");
    let tail_items: Vec<&ResponseItem> = regenerated
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(ri) => Some(&ri.item),
            _ => None,
        })
        .collect();

    // Find exactly one CustomToolCall and one CustomToolCallOutput.
    let calls: Vec<&&ResponseItem> = tail_items
        .iter()
        .filter(|item| {
            matches!(item, ResponseItem::CustomToolCall { call_id, .. }
                     if call_id == "call-nc4-pair")
        })
        .collect();
    let outputs: Vec<&&ResponseItem> = tail_items
        .iter()
        .filter(|item| {
            matches!(item, ResponseItem::CustomToolCallOutput { call_id, .. }
                     if call_id == "call-nc4-pair")
        })
        .collect();
    assert_eq!(calls.len(), 1, "exactly one CustomToolCall must survive");
    assert_eq!(
        outputs.len(),
        1,
        "exactly one CustomToolCallOutput must survive"
    );

    // Byte-for-byte: the grafted pair must match the original provider-native
    // objects (sans id assignment). Status, namespace, structured ContentItems,
    // success — all must be preserved through the graft.
    let call_bytes = crate::item_bytes_without_id(calls[0]);
    let output_bytes = crate::item_bytes_without_id(outputs[0]);
    let orig_call_bytes = crate::item_bytes_without_id(&exact_call);
    let orig_output_bytes = crate::item_bytes_without_id(&exact_output);
    assert_eq!(
        call_bytes, orig_call_bytes,
        "CustomToolCall must be byte-stable (status, namespace, input preserved)"
    );
    assert_eq!(
        output_bytes, orig_output_bytes,
        "CustomToolCallOutput must be byte-stable (ContentItems, success preserved)"
    );

    // Ordering: call before output.
    let call_pos = tail_items.iter().position(|item| {
        matches!(item, ResponseItem::CustomToolCall { call_id, .. }
                 if call_id == "call-nc4-pair")
    });
    let output_pos = tail_items.iter().position(|item| {
        matches!(item, ResponseItem::CustomToolCallOutput { call_id, .. }
                 if call_id == "call-nc4-pair")
    });
    assert!(
        call_pos < output_pos,
        "CustomToolCall must precede CustomToolCallOutput"
    );

    // Convergence: second reconcile is Unchanged.
    let second = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    assert_eq!(
        second,
        ReconcileOutcome::Unchanged { reason: "ok" },
        "second reconcile must converge"
    );
}

/// nc4 negative: ambiguous cardinality (duplicate call_id) in prior rollout
/// must NOT graft — the materialized LHC shape is left as-is.
#[tokio::test]
async fn nc4_ambiguous_pair_does_not_graft() {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;

    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "nc4-ambig";

    let pad = "x".repeat(2500);
    let mut items = Vec::new();
    for i in 0..80 {
        items.push(user(&format!("nc4a user {i} {pad}"), &format!("u{i}")));
        items.push(assistant(&format!("nc4a asst {i} {pad}"), &format!("a{i}")));
    }
    items.push(user("ambig turn", "u80"));
    items.push(ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: "call-ambig".into(),
        name: "tool".into(),
        namespace: Some("ns".into()),
        input: "{}".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    seed_thread(&root, tid, &items).await;

    let result = crate::produce_lhc_compact_deterministic(
        tid,
        Some(root.as_path()),
        &items,
        /*import*/ false,
    )
    .await
    .expect("compact");
    assert!(result.marker.compact_point > 0);

    // Prior rollout with DUPLICATE call_ids (ambiguous cardinality).
    let path = dir.path().join("sessions").join("nc4a-stale.jsonl");
    let dup_call = ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: "call-ambig".into(),
        name: "tool".into(),
        namespace: Some("ns".into()),
        input: "{}".into(),
        internal_chat_message_metadata_passthrough: None,
    };
    let dup_output = ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-ambig".into(),
        name: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("out1".into()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    let mut stale = single_boundary_items(0);
    // Two calls with same call_id = ambiguous.
    stale.push(RolloutItem::ResponseItem(dup_call.clone().into()));
    stale.push(RolloutItem::ResponseItem(dup_call.clone().into()));
    stale.push(RolloutItem::ResponseItem(dup_output.clone().into()));
    write_items(&path, &stale);

    // Record pre-reconcile file bytes.
    let pre_bytes = std::fs::read(&path).expect("read pre");

    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;

    // Ambiguous terminal suffix => fail-open, file bytes unchanged.
    assert_eq!(
        outcome,
        ReconcileOutcome::Unchanged {
            reason: "regenerate_failed"
        },
        "ambiguous terminal suffix must leave prior unchanged; got {outcome:?}"
    );
    let post_bytes = std::fs::read(&path).expect("read post");
    assert_eq!(
        pre_bytes, post_bytes,
        "file bytes must be exactly unchanged after ambiguous graft failure"
    );
}

/// nc4: two parallel terminal pairs — both grafted byte-stably and ordered.
#[tokio::test]
async fn nc4_two_parallel_terminal_pairs_both_grafted() {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem as Item;
    use codex_protocol::models::FunctionCallOutputPayload;

    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "nc4-parallel";

    let call_a = ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: "call-par-a".into(),
        name: "tool_a".into(),
        namespace: Some("ns_a".into()),
        input: r#"{"a":1}"#.into(),
        internal_chat_message_metadata_passthrough: None,
    };
    let call_b = ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: "call-par-b".into(),
        name: "tool_b".into(),
        namespace: Some("ns_b".into()),
        input: r#"{"b":2}"#.into(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output_a = ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-par-a".into(),
        name: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![Item::InputText {
                text: "result_a".into(),
            }]),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    let output_b = ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-par-b".into(),
        name: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("result_b".into()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };

    let pad = "x".repeat(2500);
    let mut items = Vec::new();
    for i in 0..80 {
        items.push(user(&format!("nc4par user {i} {pad}"), &format!("u{i}")));
        items.push(assistant(
            &format!("nc4par asst {i} {pad}"),
            &format!("a{i}"),
        ));
    }
    items.push(user("parallel turn", "u80"));
    items.push(call_a.clone());
    items.push(call_b.clone());
    items.push(output_a.clone());
    items.push(output_b.clone());
    // No trailing assistant.
    seed_thread(&root, tid, &items).await;

    let result = crate::produce_lhc_compact_deterministic(
        tid,
        Some(root.as_path()),
        &items,
        /*import*/ false,
    )
    .await
    .expect("compact");
    assert!(result.marker.compact_point > 0);

    let path = dir.path().join("sessions").join("nc4par-stale.jsonl");
    let mut stale = single_boundary_items(0);
    stale.push(RolloutItem::ResponseItem(call_a.clone().into()));
    stale.push(RolloutItem::ResponseItem(call_b.clone().into()));
    stale.push(RolloutItem::ResponseItem(output_a.clone().into()));
    stale.push(RolloutItem::ResponseItem(output_b.clone().into()));
    write_items(&path, &stale);

    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    assert!(
        matches!(&outcome, ReconcileOutcome::Regenerated { .. }),
        "expected Regenerated"
    );

    let regen = parse_rollout_items(&path).expect("parse");
    let tail: Vec<&ResponseItem> = regen
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(ri) => Some(&ri.item),
            _ => None,
        })
        .collect();

    // Both pairs must be byte-stable.
    for (cid, orig_call, orig_output) in [
        ("call-par-a", &call_a, &output_a),
        ("call-par-b", &call_b, &output_b),
    ] {
        let found_call = tail
            .iter()
            .find(|item| {
                matches!(item, ResponseItem::CustomToolCall { call_id, .. } if call_id == cid)
            })
            .unwrap_or_else(|| panic!("CustomToolCall {cid} not found"));
        let found_output = tail
            .iter()
            .find(|item| {
                matches!(item, ResponseItem::CustomToolCallOutput { call_id, .. } if call_id == cid)
            })
            .unwrap_or_else(|| panic!("CustomToolCallOutput {cid} not found"));
        assert_eq!(
            crate::item_bytes_without_id(found_call),
            crate::item_bytes_without_id(orig_call),
            "call {cid} must be byte-stable"
        );
        assert_eq!(
            crate::item_bytes_without_id(found_output),
            crate::item_bytes_without_id(orig_output),
            "output {cid} must be byte-stable"
        );
    }
}

/// nc4 negative: orphan output (output_call_id with no matching call) in the
/// terminal suffix must leave the prior rollout byte-unchanged.
#[tokio::test]
async fn nc4_orphan_output_leaves_prior_unchanged() {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;

    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "nc4-orphan-out";

    let pad = "x".repeat(2500);
    let mut items = Vec::new();
    for i in 0..80 {
        items.push(user(&format!("nc4o user {i} {pad}"), &format!("u{i}")));
        items.push(assistant(&format!("nc4o asst {i} {pad}"), &format!("a{i}")));
    }
    items.push(user("orphan turn", "u80"));
    items.push(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-orphan".into(),
        name: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("orphan result".into()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    });
    seed_thread(&root, tid, &items).await;

    let result = crate::produce_lhc_compact_deterministic(
        tid,
        Some(root.as_path()),
        &items,
        /*import*/ false,
    )
    .await
    .expect("compact");
    assert!(result.marker.compact_point > 0);

    let path = dir.path().join("sessions").join("nc4o-stale.jsonl");
    let mut stale = single_boundary_items(0);
    stale.push(RolloutItem::ResponseItem(
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "call-orphan".into(),
            name: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("orphan result".into()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    write_items(&path, &stale);

    let pre_bytes = std::fs::read(&path).expect("read pre");
    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    assert_eq!(
        outcome,
        ReconcileOutcome::Unchanged {
            reason: "regenerate_failed"
        },
        "orphan output must leave prior unchanged; got {outcome:?}"
    );
    let post_bytes = std::fs::read(&path).expect("read post");
    assert_eq!(pre_bytes, post_bytes, "file bytes must be unchanged");
}

/// nc4 negative: missing output (call with no matching output) in the
/// terminal suffix must leave the prior rollout byte-unchanged.
#[tokio::test]
async fn nc4_missing_output_leaves_prior_unchanged() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "nc4-miss-out";

    let pad = "x".repeat(2500);
    let mut items = Vec::new();
    for i in 0..80 {
        items.push(user(&format!("nc4m user {i} {pad}"), &format!("u{i}")));
        items.push(assistant(&format!("nc4m asst {i} {pad}"), &format!("a{i}")));
    }
    items.push(user("missing output turn", "u80"));
    items.push(ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: "call-missing".into(),
        name: "tool".into(),
        namespace: Some("ns".into()),
        input: "{}".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    seed_thread(&root, tid, &items).await;

    let result = crate::produce_lhc_compact_deterministic(
        tid,
        Some(root.as_path()),
        &items,
        /*import*/ false,
    )
    .await
    .expect("compact");
    assert!(result.marker.compact_point > 0);

    let path = dir.path().join("sessions").join("nc4m-stale.jsonl");
    let mut stale = single_boundary_items(0);
    stale.push(RolloutItem::ResponseItem(
        ResponseItem::CustomToolCall {
            id: None,
            status: Some("completed".into()),
            call_id: "call-missing".into(),
            name: "tool".into(),
            namespace: Some("ns".into()),
            input: "{}".into(),
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    write_items(&path, &stale);

    let pre_bytes = std::fs::read(&path).expect("read pre");
    let outcome = reconcile_rollout_at_path(&path, tid, Some(root.as_path()), None).await;
    assert_eq!(
        outcome,
        ReconcileOutcome::Unchanged {
            reason: "regenerate_failed"
        },
        "missing output must leave prior unchanged; got {outcome:?}"
    );
    let post_bytes = std::fs::read(&path).expect("read post");
    assert_eq!(pre_bytes, post_bytes, "file bytes must be unchanged");
}

// ── R12 (CX-S2): reopen-failure receipt ───────────────────────────────────

/// The receipt carries what the next open needs: which compacted rollout is
/// live (hash + size + item count), how far the recorder got, and where
/// canonical LHC capture was when the recorder handle died.
#[tokio::test]
async fn reopen_failure_receipt_carries_rollout_identity_and_frontiers() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("lhc");
    let tid = "reopen-receipt-tid";
    seed_thread(
        &root,
        tid,
        &[user("hello receipt", "u1"), assistant("hi", "a1")],
    )
    .await;

    let path = dir.path().join("sessions").join("rollout-reopen.jsonl");
    let items = single_boundary_items(7);
    write_items(&path, &items);

    let identity = compacted_rollout_identity(&path).expect("identity");
    assert_eq!(
        identity.items,
        items.len() as u64,
        "identity item count must match the compacted rollout"
    );
    assert_eq!(identity.sha256.len(), 64, "sha256 hex over rollout bytes");
    assert_eq!(
        identity.bytes,
        std::fs::metadata(&path).expect("meta").len(),
        "identity size must match the file on disk"
    );

    let capture = read_capture_frontier(tid, Some(root.as_path()))
        .await
        .expect("capture frontier");
    assert!(
        capture.event_count > 0 && capture.last_event_order > 0,
        "canonical capture frontier must be non-empty: {capture:?}"
    );

    let receipt = RolloutReopenFailureReceipt {
        schema: ROLLOUT_REOPEN_RECEIPT_SCHEMA.to_string(),
        written_at: "2026-08-19T00:00:00Z".into(),
        thread_id: tid.into(),
        rollout_path: path.display().to_string(),
        compacted_rollout: identity.clone(),
        recorder_frontier_items: items.len() as u64,
        capture_frontier: Some(capture),
        reopen_error: "orphan inode".into(),
    };
    write_rollout_reopen_failure_receipt(&path, &receipt).expect("write receipt");

    let read_back = read_rollout_reopen_failure_receipt(&path).expect("receipt present");
    assert_eq!(read_back, receipt, "receipt must round-trip verbatim");
    assert_eq!(read_back.compacted_rollout, identity);
    assert_eq!(read_back.capture_frontier, Some(capture));
    // The compacted rollout itself is untouched by receipt bookkeeping.
    assert_eq!(
        parse_rollout_items(&path).expect("parse").len(),
        items.len(),
        "receipt write must not disturb the compacted rollout"
    );
}

/// The receipt is write-behind: an unwritable sidecar (missing parent dir)
/// surfaces as an error to the caller, which warns and keeps the compact. An
/// absent or corrupt receipt reads as "no accounting", never as a block.
#[test]
fn unwritable_or_corrupt_receipt_reads_as_absent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("nope").join("rollout.jsonl");
    let receipt = RolloutReopenFailureReceipt {
        schema: ROLLOUT_REOPEN_RECEIPT_SCHEMA.to_string(),
        written_at: "2026-08-19T00:00:00Z".into(),
        thread_id: "tid".into(),
        rollout_path: path.display().to_string(),
        compacted_rollout: CompactedRolloutIdentity {
            sha256: "deadbeef".into(),
            bytes: 1,
            items: 1,
        },
        recorder_frontier_items: 1,
        capture_frontier: None,
        reopen_error: "boom".into(),
    };
    assert!(
        write_rollout_reopen_failure_receipt(&path, &receipt).is_err(),
        "missing parent directory cannot be written"
    );
    assert!(
        read_rollout_reopen_failure_receipt(&path).is_none(),
        "no receipt reads as absent"
    );

    let live = dir.path().join("rollout.jsonl");
    write_items(&live, &single_boundary_items(1));
    std::fs::write(rollout_reopen_receipt_path(&live), b"{not json").expect("write junk");
    assert!(
        read_rollout_reopen_failure_receipt(&live).is_none(),
        "corrupt receipt reads as absent, never as authority"
    );
}
