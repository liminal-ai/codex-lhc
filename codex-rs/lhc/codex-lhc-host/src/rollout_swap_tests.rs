//! Crash-injection and generation-retention tests for atomic rollout rewrite.

use super::*;
use crate::estimate_response_items_tokens;
use codex_history::CompactedItem;
use codex_history::ROLLOUT_GENERATION_ID_FIELD;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

fn sample_items(tag: &str) -> Vec<RolloutItem> {
    vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                ..SessionMeta::default()
            },
            git: None,
        }),
        RolloutItem::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![ContentItem::InputText {
                    text: format!("hello {tag}"),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
        RolloutItem::Compacted(CompactedItem {
            mcp_resource_origins: None,
            message: format!("boundary-{tag}"),
            replacement_history: Some(vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".into(),
                    content: vec![ContentItem::InputText {
                        text: format!("band {tag}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }
                .into(),
            ]),
            window_number: Some(1),
            first_window_id: Some("first".into()),
            previous_window_id: None,
            window_id: Some("win-1".into()),
        }),
    ]
}

fn write_seed(path: &Path, tag: &str) {
    write_rollout_jsonl(path, &sample_items(tag)).expect("seed write");
}

fn rollout_lines(path: &Path) -> Vec<RolloutLine> {
    std::fs::read_to_string(path)
        .expect("read rollout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse rollout line"))
        .collect()
}

fn rollout_generation_id(path: &Path) -> String {
    let values = std::fs::read_to_string(path)
        .expect("read rollout generation")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse rollout value"))
        .collect::<Vec<_>>();
    assert!(
        values[1..]
            .iter()
            .all(|value| value.get(ROLLOUT_GENERATION_ID_FIELD).is_none())
    );
    values[0][ROLLOUT_GENERATION_ID_FIELD]
        .as_str()
        .expect("SessionMeta generation ID")
        .to_string()
}

fn paginated_items(
    tag: &str,
    history_base: Option<u64>,
    subagent_history_start_ordinal: Option<u64>,
) -> Vec<RolloutItem> {
    let mut items = sample_items(tag);
    let RolloutItem::SessionMeta(meta) = &mut items[0] else {
        panic!("first item must be session metadata");
    };
    meta.meta.history_mode = ThreadHistoryMode::Paginated;
    meta.meta.history_base =
        history_base.map(
            |end_ordinal_exclusive| codex_protocol::protocol::HistoryPosition {
                thread_id: meta.meta.id,
                end_ordinal_exclusive,
                end_byte_offset: 0,
            },
        );
    meta.meta.subagent_history_start_ordinal = subagent_history_start_ordinal;
    items
}

fn assert_parseable_active(path: &Path, expect_tag: &str) {
    let items = parse_rollout_items(path).expect("parse active");
    assert!(!items.is_empty(), "active generation must be non-empty");
    let text = serde_json::to_string(&items).expect("ser");
    assert!(
        text.contains(expect_tag),
        "active generation should contain {expect_tag}: {text}"
    );
}

#[test]
fn successful_swap_rotates_one_prev_generation() {
    // Hold the failpoint lock so a concurrent crash-injection test cannot
    // leave an armed point during a clean swap.
    let _guard = SwapFailpointGuard::arm(SwapFailpoint::None);
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    write_seed(&path, "gen1");
    atomic_rewrite_rollout(&path, &sample_items("gen2")).expect("swap");
    assert_parseable_active(&path, "gen2");
    let prev = SwapPaths::for_rollout(&path).prev;
    assert!(prev.exists(), "prior generation retained as .prev");
    assert_parseable_active(&prev, "gen1");
    // Second rewrite: only one prior retained.
    atomic_rewrite_rollout(&path, &sample_items("gen3")).expect("swap2");
    assert_parseable_active(&path, "gen3");
    assert_parseable_active(&prev, "gen2");
    let gen1_text = std::fs::read_to_string(&prev).unwrap();
    assert!(
        !gen1_text.contains("gen1"),
        "exactly one prior generation; gen1 must be gone"
    );
}

#[test]
fn legacy_rewrite_remains_ordinal_free() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");

    atomic_rewrite_rollout(&path, &sample_items("legacy")).expect("legacy rewrite");

    assert!(
        rollout_lines(&path)
            .iter()
            .all(|line| line.ordinal.is_none())
    );
}

#[test]
fn root_paginated_rewrite_is_contiguous_and_repeatable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let items = paginated_items(
        "root", /*history_base*/ None, /*subagent_history_start_ordinal*/ None,
    );

    atomic_rewrite_rollout(&path, &items).expect("first rewrite");
    let first_generation_id = rollout_generation_id(&path);
    assert_eq!(
        rollout_lines(&path)
            .iter()
            .map(|line| line.ordinal)
            .collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)]
    );

    atomic_rewrite_rollout(&path, &items).expect("repeated rewrite");
    let second_generation_id = rollout_generation_id(&path);
    assert_ne!(second_generation_id, first_generation_id);
    assert_eq!(
        rollout_lines(&path)
            .iter()
            .map(|line| line.ordinal)
            .collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)]
    );
}

#[test]
fn paginated_rewrite_honors_history_base_and_subagent_boundary() {
    let dir = tempdir().unwrap();
    let base_path = dir.path().join("history-base.jsonl");
    atomic_rewrite_rollout(
        &base_path,
        &paginated_items(
            "base",
            /*history_base*/ Some(41),
            /*subagent_history_start_ordinal*/ None,
        ),
    )
    .expect("history-base rewrite");
    assert_eq!(
        rollout_lines(&base_path)
            .iter()
            .map(|line| line.ordinal)
            .collect::<Vec<_>>(),
        vec![Some(41), Some(42), Some(43)]
    );

    let subagent_path = dir.path().join("subagent.jsonl");
    atomic_rewrite_rollout(
        &subagent_path,
        &paginated_items(
            "subagent",
            /*history_base*/ None,
            /*subagent_history_start_ordinal*/ Some(8),
        ),
    )
    .expect("subagent rewrite");
    assert_eq!(
        rollout_lines(&subagent_path)
            .iter()
            .map(|line| line.ordinal)
            .collect::<Vec<_>>(),
        vec![Some(5), Some(6), Some(7)]
    );
}

#[test]
fn failpoint_post_temp_write_leaves_old_authoritative() {
    let _guard = SwapFailpointGuard::arm(SwapFailpoint::PostTempWrite);
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    write_seed(&path, "old");
    let err = atomic_rewrite_rollout(&path, &sample_items("new")).expect_err("injected");
    assert!(err.to_string().contains("PostTempWrite"), "err={err}");
    assert_parseable_active(&path, "old");
}

#[test]
fn failpoint_post_fsync_leaves_old_authoritative() {
    let _guard = SwapFailpointGuard::arm(SwapFailpoint::PostFsync);
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    write_seed(&path, "old");
    let err = atomic_rewrite_rollout(&path, &sample_items("new")).expect_err("injected");
    assert!(err.to_string().contains("PostFsync"), "err={err}");
    assert_parseable_active(&path, "old");
}

#[test]
fn failpoint_post_old_rename_leaves_prior_or_new_parseable_never_torn_active() {
    let _guard = SwapFailpointGuard::arm(SwapFailpoint::PostOldRename);
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    write_seed(&path, "old");
    let err = atomic_rewrite_rollout(&path, &sample_items("new")).expect_err("injected");
    assert!(err.to_string().contains("PostOldRename"), "err={err}");
    let paths = SwapPaths::for_rollout(&path);
    // Active was renamed to prev; temp holds the new generation.
    assert!(
        !paths.active.exists(),
        "active path empty after old rename before new rename"
    );
    assert_parseable_active(&paths.prev, "old");
    assert_parseable_active(&paths.temp, "new");
}

#[test]
fn failpoint_post_new_rename_pre_reopen_leaves_new_active_parseable() {
    let _guard = SwapFailpointGuard::arm(SwapFailpoint::PostNewRenamePreReopen);
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    write_seed(&path, "old");
    let err = atomic_rewrite_rollout(&path, &sample_items("new")).expect_err("injected");
    assert!(
        err.to_string().contains("PostNewRenamePreReopen"),
        "err={err}"
    );
    // New generation is already live; reopen is a separate step.
    assert_parseable_active(&path, "new");
    assert_parseable_active(&SwapPaths::for_rollout(&path).prev, "old");
}

#[test]
#[cfg(unix)]
fn read_only_dir_leaves_old_authoritative() {
    let _guard = SwapFailpointGuard::arm(SwapFailpoint::None);
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    write_seed(&path, "old");
    // Make the directory read-only so temp create / rename fails.
    let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(dir.path(), perms.clone()).unwrap();
    let result = atomic_rewrite_rollout(&path, &sample_items("new"));
    // Restore perms so tempdir cleanup works.
    let mut writable = perms;
    writable.set_readonly(false);
    std::fs::set_permissions(dir.path(), writable).unwrap();
    assert!(result.is_err(), "read-only dir must fail the rewrite");
    assert_parseable_active(&path, "old");
}

#[test]
fn history_from_materialized_is_bands_plus_native_tail() {
    let band = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "band body".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let tail = ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: "native tail".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta::default(),
            git: None,
        }),
        // Pre-boundary band stream item (ignored for install history — only
        // Compacted.replacement_history is the base).
        RolloutItem::ResponseItem(band.clone().into()),
        RolloutItem::Compacted(CompactedItem {
            message: "m".into(),
            replacement_history: Some(vec![band.clone().into()]),
            mcp_resource_origins: None,
            window_number: Some(2),
            first_window_id: Some("f".into()),
            previous_window_id: None,
            window_id: Some("w".into()),
        }),
        RolloutItem::ResponseItem(tail.clone().into()),
    ];
    let history = history_from_materialized_items(&items);
    assert_eq!(history, vec![band, tail]);
}

#[test]
fn dual_format_history_picks_newest_compacted_only() {
    // Old-shape: Compacted1 + tail1 + Compacted2 + tail2 → bands2 + tail2 only.
    let band1 = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "band-v1".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let band2 = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "band-v2".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let after1 = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "after-c1".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let after2 = ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: "after-c2".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta::default(),
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: "c1".into(),
            replacement_history: Some(vec![band1.into()]),
            mcp_resource_origins: None,
            window_number: Some(1),
            first_window_id: Some("f".into()),
            previous_window_id: None,
            window_id: Some("w1".into()),
        }),
        RolloutItem::ResponseItem(after1.into()),
        RolloutItem::Compacted(CompactedItem {
            message: "c2".into(),
            replacement_history: Some(vec![band2.clone().into()]),
            mcp_resource_origins: None,
            window_number: Some(2),
            first_window_id: Some("f".into()),
            previous_window_id: Some("w1".into()),
            window_id: Some("w2".into()),
        }),
        RolloutItem::ResponseItem(after2.clone().into()),
    ];
    let history = history_from_materialized_items(&items);
    assert_eq!(history, vec![band2, after2]);
    let text = format!("{history:?}");
    assert!(
        !text.contains("band-v1") && !text.contains("after-c1"),
        "first generation must not leak: {text}"
    );
}

#[test]
fn fl4_model_context_estimate_like_for_like() {
    // Body smaller or equal to rollout model-context → not pathology.
    // Body larger → genuine growth (loud-fail candidate).
    let band = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "x".repeat(400), // ~100 tokens at char/4
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta::default(),
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: "m".into(),
            replacement_history: Some(vec![band.into()]),
            mcp_resource_origins: None,
            window_number: Some(1),
            first_window_id: Some("f".into()),
            previous_window_id: None,
            window_id: Some("w".into()),
        }),
    ];
    let baseline = model_context_token_estimate_from_rollout_items(&items);
    assert!(baseline > 0, "baseline must be positive");
    let smaller = estimate_response_items_tokens(&[ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "tiny".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }]);
    assert!(
        smaller <= baseline,
        "smaller body is not pathology: {smaller} vs {baseline}"
    );
    let larger = estimate_response_items_tokens(&[ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "y".repeat(4000),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }]);
    assert!(
        larger > baseline,
        "larger body is pathology: {larger} vs {baseline}"
    );
}

#[test]
fn mutation_history_extract_drops_tail_without_boundary_split() {
    // Mutation demo: if we incorrectly took *all* ResponseItems, pre-boundary
    // band stream + tail would double the band. The real helper must not.
    let band = ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "only once".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let items = vec![
        RolloutItem::ResponseItem(band.clone().into()),
        RolloutItem::Compacted(CompactedItem {
            message: "m".into(),
            replacement_history: Some(vec![band.clone().into()]),
            mcp_resource_origins: None,
            window_number: Some(1),
            first_window_id: Some("f".into()),
            previous_window_id: None,
            window_id: Some("w".into()),
        }),
    ];
    let history = history_from_materialized_items(&items);
    assert_eq!(
        history.len(),
        1,
        "band appears once via replacement_history"
    );
    assert_eq!(history[0], band);
}
