//! Image capture and restart use the production worker and rollout materializer.
use super::*;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test]
async fn copied_schema12_preserves_text_ids_and_step_indices() {
    use lhc::OpResult;
    use lhc::shared_tech::storage::open_database;
    let dir = tempdir().expect("tempdir");
    let seed_dir = tempdir().expect("seed tempdir");
    let tid = "schema12-copy";
    let handle = spawn_capture(
        tid,
        None,
        Some(seed_dir.path().to_path_buf()),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("callbacks"),
        ),
    )
    .await
    .expect("capture");
    handle.persist(
        &user_msg("legacy [image:https://example.invalid/old.png]"),
        RawItemProvenance::UserPrompt,
        None,
    );
    let assistant: ResponseItem = serde_json::from_value(json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"legacy answer"}]})).expect("assistant");
    handle.persist(&assistant, RawItemProvenance::ModelOutput, Some(7));
    handle.flush().await;
    handle.shutdown().await;
    let source = codex_lhc_host::thread_file_path(dir.path(), tid);
    let registry_path = dir.path().join("registry.sqlite");
    for (seed, archive) in [
        (
            codex_lhc_host::thread_file_path(seed_dir.path(), tid),
            source.clone(),
        ),
        (
            seed_dir.path().join("registry.sqlite"),
            registry_path.clone(),
        ),
    ] {
        std::fs::create_dir_all(archive.parent().expect("archive parent"))
            .expect("archive directory");
        let OpResult::Ok { value: seed_db } = open_database(seed.to_str().expect("path")) else {
            panic!("open seed")
        };
        seed_db
            .prepare("VACUUM INTO ?")
            .run(&[archive.to_str().expect("path").into()]);
        seed_db.close();
    }
    let OpResult::Ok { value: registry } = open_database(registry_path.to_str().expect("path"))
    else {
        panic!("open archive registry")
    };
    registry
        .prepare("UPDATE threads SET file_path = ?")
        .run(&[source.to_str().expect("path").into()]);
    registry.close();
    let OpResult::Ok { value: db } = open_database(source.to_str().expect("path")) else {
        panic!("open fixture")
    };
    // Reverse only the additive schema-13 blob migration on a text-only fixture.
    db.exec("DROP TABLE blob; PRAGMA user_version = 12;");
    // The sticky parts witness must survive even if the latest view is full.
    db.exec("UPDATE thread_metadata SET parts_activated_at = '2026-09-05T00:00:00Z' WHERE id = 1");
    let metadata = db.prepare("SELECT * FROM thread_metadata").all(&[]);
    let messages = db
        .prepare("SELECT * FROM message ORDER BY message_id")
        .all(&[]);
    let turns = db.prepare("SELECT * FROM turns ORDER BY turn_id").all(&[]);
    assert!(
        messages
            .iter()
            .any(|m| m.get("step_index") == Some(&json!(7)))
    );
    let copied = dir.path().join("copied.sqlite");
    db.prepare("VACUUM INTO ?")
        .run(&[copied.to_str().expect("path").into()]);
    db.close();
    let OpResult::Ok { value: migrated } =
        lhc::threads::open_thread_database(copied.to_str().expect("path"))
    else {
        panic!("migrate copy")
    };
    assert_eq!(
        migrated
            .prepare("SELECT * FROM message ORDER BY message_id")
            .all(&[]),
        messages
    );
    assert_eq!(
        migrated
            .prepare("SELECT * FROM turns ORDER BY turn_id")
            .all(&[]),
        turns
    );
    assert_eq!(
        migrated.prepare("SELECT * FROM thread_metadata").all(&[]),
        metadata
    );
    assert!(matches!(
        lhc::shared_tech::storage::get_schema_version(&migrated),
        OpResult::Ok { value: 13 }
    ));
    migrated.close();
    let OpResult::Ok { value: original } = open_database(source.to_str().expect("path")) else {
        panic!("open original")
    };
    assert!(matches!(
        lhc::shared_tech::storage::get_schema_version(&original),
        OpResult::Ok { value: 12 }
    ));
    original.close();
    let items = codex_lhc_host::materialize_thread_rollout_items(
        &dir.path().join("legacy.jsonl"),
        tid,
        Some(dir.path()),
        codex_lhc_host::RolloutReconcileTrigger::Missing,
        None,
    )
    .await
    .expect("legacy text reconstruction");
    let history = codex_lhc_host::history_from_materialized_items(&items);
    assert!(history.iter().any(|item| matches!(item, ResponseItem::Message { content, .. }
        if content == &vec![ContentItem::InputText { text: "legacy [image:https://example.invalid/old.png]".into() }] )));
}

#[tokio::test]
async fn image_tool_result_preserves_order_detail_and_pair_after_restart() {
    let dir = tempdir().expect("tempdir");
    let tid = "image-tool-restart";
    let handle = spawn_capture(
        tid,
        None,
        Some(dir.path().to_path_buf()),
        codex_lhc_host::LateBoundCallbacks::seeded(
            codex_lhc_host::lhc_inference_callbacks(false).expect("callbacks"),
        ),
    )
    .await
    .expect("capture");
    let output = FunctionCallOutputPayload {
        success: Some(true),
        body: FunctionCallOutputBody::ContentItems(vec![
            FunctionCallOutputContentItem::InputText {
                text: "before".into(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,YWJj".into(),
                detail: Some(ImageDetail::Original),
            },
            FunctionCallOutputContentItem::InputText {
                text: "between".into(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "https://example.invalid/image.png".into(),
                detail: Some(ImageDetail::Low),
            },
            FunctionCallOutputContentItem::InputText {
                text: "after".into(),
            },
        ]),
    };
    let call: ResponseItem = serde_json::from_value(json!({"type":"function_call", "call_id":"image-call", "name":"view_image", "arguments":"{}"})).expect("call");
    let result = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("image-call".into()),
        name: None,
        namespace: None,
        output: output.clone(),
        internal_chat_message_metadata_passthrough: None,
    };
    handle.persist(
        &user_msg("inspect image"),
        RawItemProvenance::UserPrompt,
        None,
    );
    handle.persist(&call, RawItemProvenance::ModelOutput, None);
    handle.persist(&result, RawItemProvenance::ModelOutput, None);
    handle.flush().await;
    let events = handle.list_events().await.expect("events");
    assert!(
        !serde_json::to_string(&events)
            .expect("serialize")
            .contains("YWJj"),
        "binary payload belongs in the blob store"
    );
    handle.shutdown().await;
    let items = codex_lhc_host::materialize_thread_rollout_items(
        &dir.path().join("restart.jsonl"),
        tid,
        Some(dir.path()),
        codex_lhc_host::RolloutReconcileTrigger::Missing,
        None,
    )
    .await
    .expect("restart materialization");
    let history = codex_lhc_host::history_from_materialized_items(&items);
    assert!(history.iter().any(
        |item| matches!(item, ResponseItem::FunctionCall { call_id, .. } if call_id == "image-call")
    ));
    let restored = history
        .iter()
        .find_map(|item| match item {
            ResponseItem::FunctionCallOutput {
                call_id: Some(id),
                output,
                ..
            } if id == "image-call" => Some(output),
            _ => None,
        })
        .expect("paired output");
    assert_eq!(restored, &output);
    let file = codex_lhc_host::thread_file_path(dir.path(), tid);
    let lhc::OpResult::Ok { value: db } =
        lhc::shared_tech::storage::open_database(file.to_str().expect("path"))
    else {
        panic!("open blob store")
    };
    db.exec("DELETE FROM blob");
    db.close();
    let missing = codex_lhc_host::materialize_thread_rollout_items(
        &dir.path().join("missing.jsonl"),
        tid,
        Some(dir.path()),
        codex_lhc_host::RolloutReconcileTrigger::Missing,
        None,
    )
    .await
    .expect("missing blob reconstruction");
    let history = codex_lhc_host::history_from_materialized_items(&missing);
    let restored = history
        .iter()
        .find_map(|item| match item {
            ResponseItem::FunctionCallOutput { output, .. } => Some(output),
            _ => None,
        })
        .expect("tool output with missing blob");
    let FunctionCallOutputBody::ContentItems(parts) = &restored.body else {
        panic!("ordered parts")
    };
    assert!(
        matches!(&parts[1], FunctionCallOutputContentItem::InputText { text } if text.contains("image") && text.len() < 100)
    );
    assert!(
        matches!(&parts[3], FunctionCallOutputContentItem::InputImage { image_url, .. } if image_url == "https://example.invalid/image.png")
    );
}
