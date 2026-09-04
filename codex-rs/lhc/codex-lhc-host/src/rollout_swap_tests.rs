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
            guardian_history: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
    ]
}

fn write_seed(path: &Path, tag: &str) {
    write_rollout_jsonl(path, &sample_items(tag), &new_rollout_generation_id())
        .expect("seed write");
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
            guardian_history: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
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
            guardian_history: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
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
            guardian_history: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
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
            guardian_history: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
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
            guardian_history: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
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

/// Exact generations for a seed of `sample_items("old")` swapped to
/// `sample_items("new")`: prior bytes captured from the active file before
/// the swap, new items as written.
struct Gens {
    prior: Vec<u8>,
    new_items: Vec<RolloutItem>,
    /// The identity this attempt writes, generated up front and retained.
    generation_id: String,
}

impl Gens {
    fn capture(path: &Path) -> Self {
        Self {
            prior: std::fs::read(path).expect("prior bytes"),
            new_items: sample_items("new"),
            generation_id: new_rollout_generation_id(),
        }
    }

    fn as_swap(&self) -> SwapGenerations<'_> {
        SwapGenerations {
            prior_bytes: Some(self.prior.as_slice()),
            new_items: &self.new_items,
            new_generation_id: &self.generation_id,
        }
    }

    fn rewrite(&self, path: &Path) -> std::io::Result<()> {
        atomic_rewrite_rollout_as_generation(path, &self.new_items, &self.generation_id)
    }
}

fn assert_exact_new(path: &Path, gens: &Gens) {
    proves_new_generation(path, &gens.new_items, &gens.generation_id)
        .unwrap_or_else(|err| panic!("active must be exactly the new generation: {err}"));
}

/// Interrupted-swap reconciliation establishes one authoritative active
/// generation per injected stage: old still active (retry later), old moved
/// with the proven new generation at tmp (finish the swap), new already
/// active (nothing to move; caller completes its install), old moved with an
/// unproven tmp (restore the byte-exact prior), nothing provable (reported).
#[test]
fn reconcile_interrupted_swap_establishes_one_active_generation() {
    // PostTempWrite: old stays active, nothing moved.
    {
        let _guard = SwapFailpointGuard::arm(SwapFailpoint::PostTempWrite);
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        gens.rewrite(&path).expect_err("injected");
        assert_eq!(
            classify_swap_state(&path, gens.as_swap()),
            SwapState::OldActive
        );
        assert_eq!(
            reconcile_interrupted_swap(&path, gens.as_swap()),
            SwapReconciliation::OldActive
        );
        assert_eq!(std::fs::read(&path).unwrap(), gens.prior);
    }
    // PostOldRename: no active; the proven new generation at tmp is moved
    // into place and the old generation stays at prev.
    {
        let _guard = SwapFailpointGuard::arm(SwapFailpoint::PostOldRename);
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        gens.rewrite(&path).expect_err("injected");
        assert_eq!(
            classify_swap_state(&path, gens.as_swap()),
            SwapState::NoActive {
                prev_exists: true,
                temp_exists: true
            }
        );
        assert_eq!(
            reconcile_interrupted_swap(&path, gens.as_swap()),
            SwapReconciliation::NewActive {
                finished_here: true
            }
        );
        let paths = SwapPaths::for_rollout(&path);
        assert!(!paths.temp.exists(), "tmp consumed by the finished swap");
        assert_eq!(
            std::fs::read(&paths.prev).unwrap(),
            gens.prior,
            "old generation retained byte-exactly at prev"
        );
        assert_eq!(
            classify_swap_state(&path, gens.as_swap()),
            SwapState::NewActive
        );
        assert_exact_new(&path, &gens);
    }
    // PostNewRenamePreReopen: new already active; reconciliation moves nothing.
    {
        let _guard = SwapFailpointGuard::arm(SwapFailpoint::PostNewRenamePreReopen);
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        gens.rewrite(&path).expect_err("injected");
        assert_eq!(
            reconcile_interrupted_swap(&path, gens.as_swap()),
            SwapReconciliation::NewActive {
                finished_here: false
            }
        );
        assert_exact_new(&path, &gens);
    }
    // Old moved, tmp unproven (torn): restore the byte-exact prior generation.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        let paths = SwapPaths::for_rollout(&path);
        std::fs::rename(&paths.active, &paths.prev).unwrap();
        std::fs::write(&paths.temp, "{not a rollout line\n").unwrap();
        let outcome = reconcile_interrupted_swap(&path, gens.as_swap());
        assert!(
            matches!(outcome, SwapReconciliation::RestoredOld { .. }),
            "{outcome:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), gens.prior);
        assert!(!paths.prev.exists());
    }
    // Nothing to restore: reported, never guessed.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let new_items = sample_items("new");
        let outcome = reconcile_interrupted_swap(
            &path,
            SwapGenerations {
                prior_bytes: None,
                new_items: &new_items,
                new_generation_id: &new_rollout_generation_id(),
            },
        );
        assert!(
            matches!(outcome, SwapReconciliation::Unreconciled { .. }),
            "{outcome:?}"
        );
    }
}

/// Torn/foreign content never becomes authoritative: a parseable foreign
/// active file, a torn active file that the tolerant reader would still
/// accept, a foreign or torn `.prev`, a parseable-but-foreign tmp, and a
/// failed directory sync after a repair rename each refuse promotion and
/// report `Unreconciled` with the exact state, leaving every file in place.
#[test]
fn reconcile_interrupted_swap_refuses_unproven_generations() {
    let paths_of = SwapPaths::for_rollout;
    // Parseable foreign active: neither generation.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        write_rollout_jsonl(
            &path,
            &sample_items("foreign"),
            &new_rollout_generation_id(),
        )
        .unwrap();
        let foreign = std::fs::read(&path).unwrap();
        assert!(
            !parse_rollout_items(&path).unwrap().is_empty(),
            "tolerant reader accepts it"
        );
        let state = classify_swap_state(&path, gens.as_swap());
        assert!(matches!(state, SwapState::Unknown { .. }), "{state:?}");
        let outcome = reconcile_interrupted_swap(&path, gens.as_swap());
        let SwapReconciliation::Unreconciled { detail } = outcome else {
            panic!("foreign active must be unreconciled: {outcome:?}");
        };
        assert!(detail.contains("proves neither generation"), "{detail}");
        assert_eq!(std::fs::read(&path).unwrap(), foreign, "left untouched");
    }
    // Torn active retaining valid rows: the tolerant reader yields the old
    // rows; the exact proof refuses.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        let mut torn = gens.prior.clone();
        torn.extend_from_slice(br#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","#);
        std::fs::write(&path, &torn).unwrap();
        assert_eq!(
            parse_rollout_items(&path).unwrap().len(),
            sample_items("old").len(),
            "tolerant reader skips the torn row and would have called this old"
        );
        assert!(strict_read_generation(&path).is_err());
        let outcome = reconcile_interrupted_swap(&path, gens.as_swap());
        let SwapReconciliation::Unreconciled { detail } = outcome else {
            panic!("torn active must be unreconciled: {outcome:?}");
        };
        assert!(detail.contains("bytes != prior generation"), "{detail}");
        assert!(detail.contains("not newline-terminated"), "{detail}");
        assert_eq!(std::fs::read(&path).unwrap(), torn, "left untouched");
    }
    // Foreign prev (no active, no tmp): never restored.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        let paths = paths_of(&path);
        std::fs::remove_file(&path).unwrap();
        write_rollout_jsonl(
            &paths.prev,
            &sample_items("foreign"),
            &new_rollout_generation_id(),
        )
        .unwrap();
        let foreign = std::fs::read(&paths.prev).unwrap();
        let outcome = reconcile_interrupted_swap(&path, gens.as_swap());
        let SwapReconciliation::Unreconciled { detail } = outcome else {
            panic!("foreign prev must not be restored: {outcome:?}");
        };
        assert!(
            detail.contains("prev does not prove the prior generation"),
            "{detail}"
        );
        assert!(!path.exists(), "nothing promoted to active");
        assert_eq!(
            std::fs::read(&paths.prev).unwrap(),
            foreign,
            "prev untouched"
        );
    }
    // Torn prev with valid rows + parseable foreign tmp: neither promoted.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        let paths = paths_of(&path);
        std::fs::rename(&path, &paths.prev).unwrap();
        let mut torn = gens.prior.clone();
        torn.extend_from_slice(b"{\"timestamp\":\"x\"");
        std::fs::write(&paths.prev, &torn).unwrap();
        write_rollout_jsonl(
            &paths.temp,
            &sample_items("foreign"),
            &new_rollout_generation_id(),
        )
        .unwrap();
        let foreign_tmp = std::fs::read(&paths.temp).unwrap();
        let outcome = reconcile_interrupted_swap(&path, gens.as_swap());
        let SwapReconciliation::Unreconciled { detail } = outcome else {
            panic!("torn prev / foreign tmp must not be promoted: {outcome:?}");
        };
        assert!(
            detail.contains("tmp does not prove the new generation"),
            "{detail}"
        );
        assert!(
            detail.contains("prev does not prove the prior generation"),
            "{detail}"
        );
        assert!(!path.exists(), "nothing promoted to active");
        assert_eq!(std::fs::read(&paths.prev).unwrap(), torn);
        assert_eq!(std::fs::read(&paths.temp).unwrap(), foreign_tmp);
    }
    // Parseable foreign tmp with a byte-exact prev: tmp refused, prev restored.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        let paths = paths_of(&path);
        std::fs::rename(&path, &paths.prev).unwrap();
        write_rollout_jsonl(
            &paths.temp,
            &sample_items("foreign"),
            &new_rollout_generation_id(),
        )
        .unwrap();
        let outcome = reconcile_interrupted_swap(&path, gens.as_swap());
        let SwapReconciliation::RestoredOld { detail } = outcome else {
            panic!("byte-exact prev must be restored: {outcome:?}");
        };
        assert!(
            detail.contains("tmp does not prove the new generation"),
            "{detail}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), gens.prior);
        assert!(paths.temp.exists(), "foreign tmp left for inspection");
    }
    // Repair sync failure after tmp → active: reported, not established; a
    // later reconciliation (sync working) finds the new generation active.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        let paths = paths_of(&path);
        {
            let _guard = SwapFailpointGuard::arm(SwapFailpoint::PostOldRename);
            gens.rewrite(&path).expect_err("injected");
        }
        let outcome = {
            let _guard = SwapFailpointGuard::arm(SwapFailpoint::ReconcileDirSync);
            reconcile_interrupted_swap(&path, gens.as_swap())
        };
        let SwapReconciliation::Unreconciled { detail } = outcome else {
            panic!("repair without a proven sync must be unreconciled: {outcome:?}");
        };
        assert!(detail.contains("directory sync failed"), "{detail}");
        assert!(detail.contains("finished tmp"), "{detail}");
        assert_eq!(std::fs::read(&paths.prev).unwrap(), gens.prior);
        assert_eq!(
            reconcile_interrupted_swap(&path, gens.as_swap()),
            SwapReconciliation::NewActive {
                finished_here: false
            }
        );
    }
    // Repair sync failure after prev → active: same disposition.
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        let paths = paths_of(&path);
        std::fs::rename(&path, &paths.prev).unwrap();
        let outcome = {
            let _guard = SwapFailpointGuard::arm(SwapFailpoint::ReconcileDirSync);
            reconcile_interrupted_swap(&path, gens.as_swap())
        };
        let SwapReconciliation::Unreconciled { detail } = outcome else {
            panic!("restore without a proven sync must be unreconciled: {outcome:?}");
        };
        assert!(detail.contains("restored prev"), "{detail}");
        assert!(detail.contains("directory sync failed"), "{detail}");
        assert_eq!(
            reconcile_interrupted_swap(&path, gens.as_swap()),
            SwapReconciliation::OldActive
        );
    }
    // Both a swap stage and a repair failure armed together (mask).
    {
        let _guard = SwapFailpointGuard::arm_all(&[
            SwapFailpoint::PostOldRename,
            SwapFailpoint::ReconcileDirSync,
        ]);
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_seed(&path, "old");
        let gens = Gens::capture(&path);
        gens.rewrite(&path).expect_err("injected");
        let outcome = reconcile_interrupted_swap(&path, gens.as_swap());
        assert!(
            matches!(outcome, SwapReconciliation::Unreconciled { .. }),
            "{outcome:?}"
        );
    }
}

type LineMutation = fn(&mut serde_json::Map<String, serde_json::Value>);

/// Rewrite line `index` (0-based) of `path` through `mutate` on its raw JSON
/// object, preserving every other line.
fn mutate_line(path: &Path, index: usize, mutate: LineMutation) {
    let text = std::fs::read_to_string(path).expect("read");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut value: serde_json::Value = serde_json::from_str(&lines[index]).expect("line json");
    mutate(value.as_object_mut().expect("object"));
    lines[index] = serde_json::to_string(&value).expect("ser");
    std::fs::write(path, format!("{}\n", lines.join("\n"))).expect("write");
}

/// Envelope derivation: the strict proof accepts exactly the envelope the
/// writer emits — no ordinals on legacy output, the `for_rewrite` contiguous
/// plan on paginated output, one UUID v4 generation id on `SessionMeta` rows
/// only, RFC 3339 timestamps — and every mutation of it (missing, duplicate,
/// shifted, reordered, malformed, or foreign ordinals; missing, foreign,
/// non-generated, misplaced, non-string, or split generation ids; arbitrary
/// or missing timestamps) is refused: `classify_swap_state` reports
/// `Unknown` and `reconcile_interrupted_swap` reports `Unreconciled`,
/// promoting nothing.
#[test]
fn strict_new_generation_proof_requires_exact_envelope() {
    let new_items = paginated_items("new", Some(10), None);
    let legacy_items = sample_items("new");
    let generation_id = new_rollout_generation_id();
    // Baseline: the writer's own output proves, for both output shapes.
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    write_seed(&path, "old");
    let prior = std::fs::read(&path).unwrap();
    write_rollout_jsonl(&path, &new_items, &generation_id).unwrap();
    let rows = strict_read_generation(&path).expect("strict");
    assert_eq!(
        rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
        vec![Some(10), Some(11), Some(12)],
        "paginated output carries the for_rewrite plan"
    );
    assert_eq!(
        rows[0].generation_id.as_deref(),
        Some(generation_id.as_str()),
        "SessionMeta carries exactly the retained identity"
    );
    assert!(rows[1..].iter().all(|r| r.generation_id.is_none()));
    let gens = SwapGenerations {
        prior_bytes: Some(prior.as_slice()),
        new_items: &new_items,
        new_generation_id: &generation_id,
    };
    assert_eq!(classify_swap_state(&path, gens), SwapState::NewActive);
    write_rollout_jsonl(&path, &legacy_items, &generation_id).unwrap();
    assert!(
        strict_read_generation(&path)
            .unwrap()
            .iter()
            .all(|r| r.ordinal.is_none()),
        "legacy output carries no ordinals"
    );
    let legacy_gens = SwapGenerations {
        prior_bytes: Some(prior.as_slice()),
        new_items: &legacy_items,
        new_generation_id: &generation_id,
    };
    assert_eq!(
        classify_swap_state(&path, legacy_gens),
        SwapState::NewActive
    );

    let mutations: Vec<(&str, &[RolloutItem], usize, LineMutation, &str)> = vec![
        (
            "missing ordinal",
            &new_items,
            1,
            |o| {
                o.remove("ordinal");
            },
            "ordinal None != expected Some(11)",
        ),
        (
            "duplicate ordinal",
            &new_items,
            2,
            |o| {
                o.insert("ordinal".into(), 11.into());
            },
            "ordinal Some(11) != expected Some(12)",
        ),
        (
            "shifted ordinal",
            &new_items,
            0,
            |o| {
                o.insert("ordinal".into(), 9.into());
            },
            "ordinal Some(9) != expected Some(10)",
        ),
        (
            "reordered ordinals",
            &new_items,
            1,
            |o| {
                o.insert("ordinal".into(), 12.into());
            },
            "ordinal Some(12) != expected Some(11)",
        ),
        (
            "malformed ordinal (string)",
            &new_items,
            1,
            |o| {
                o.insert("ordinal".into(), "11".into());
            },
            "not a rollout line",
        ),
        (
            "malformed ordinal (negative)",
            &new_items,
            1,
            |o| {
                o.insert("ordinal".into(), (-1).into());
            },
            "not a rollout line",
        ),
        (
            "ordinal on legacy output",
            &legacy_items,
            0,
            |o| {
                o.insert("ordinal".into(), 0.into());
            },
            "ordinal Some(0) != expected None",
        ),
        (
            "missing generation id",
            &new_items,
            0,
            |o| {
                o.remove(ROLLOUT_GENERATION_ID_FIELD);
            },
            "SessionMeta row without rollout_generation_id",
        ),
        (
            "foreign generation id",
            &new_items,
            0,
            |o| {
                o.insert(ROLLOUT_GENERATION_ID_FIELD.into(), "gen-foreign".into());
            },
            "is not a generated identity",
        ),
        (
            "valid but different uuid v4",
            &new_items,
            0,
            |o| {
                o.insert(
                    ROLLOUT_GENERATION_ID_FIELD.into(),
                    new_rollout_generation_id().into(),
                );
            },
            "differs from the expected generation",
        ),
        (
            "non-generated (nil) uuid",
            &new_items,
            0,
            |o| {
                o.insert(
                    ROLLOUT_GENERATION_ID_FIELD.into(),
                    uuid::Uuid::nil().to_string().into(),
                );
            },
            "is not a generated identity",
        ),
        (
            "generation id not a string",
            &new_items,
            0,
            |o| {
                o.insert(ROLLOUT_GENERATION_ID_FIELD.into(), 7.into());
            },
            "is not a string",
        ),
        (
            "generation id on a non-SessionMeta row",
            &new_items,
            1,
            |o| {
                o.insert(
                    ROLLOUT_GENERATION_ID_FIELD.into(),
                    uuid::Uuid::new_v4().to_string().into(),
                );
            },
            "on a non-SessionMeta row",
        ),
        (
            "arbitrary timestamp text",
            &new_items,
            2,
            |o| {
                o.insert("timestamp".into(), "yesterday".into());
            },
            "timestamp is not RFC 3339",
        ),
        (
            "missing timestamp",
            &new_items,
            2,
            |o| {
                o.remove("timestamp");
            },
            "not a rollout line",
        ),
    ];
    for (name, items, line, mutate, expect) in mutations {
        write_rollout_jsonl(&path, items, &generation_id).unwrap();
        mutate_line(&path, line, mutate);
        let mutated = std::fs::read(&path).unwrap();
        assert!(
            !parse_rollout_items(&path).unwrap().is_empty(),
            "{name}: the tolerant reader still yields rows"
        );
        let proof = proves_new_generation(&path, items, &generation_id).expect_err(name);
        assert!(proof.contains(expect), "{name}: {proof}");
        let gens = SwapGenerations {
            prior_bytes: Some(prior.as_slice()),
            new_items: items,
            new_generation_id: &generation_id,
        };
        let state = classify_swap_state(&path, gens);
        let SwapState::Unknown { detail } = state else {
            panic!("{name}: must be Unknown, got {state:?}");
        };
        assert!(detail.contains(expect), "{name}: {detail}");
        let outcome = reconcile_interrupted_swap(&path, gens);
        assert!(
            matches!(outcome, SwapReconciliation::Unreconciled { .. }),
            "{name}: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            mutated,
            "{name}: left untouched"
        );
    }
    // Split generation identity: two SessionMeta rows with different ids.
    {
        let mut two_meta = new_items.clone();
        two_meta.push(two_meta[0].clone());
        write_rollout_jsonl(&path, &two_meta, &generation_id).unwrap();
        proves_new_generation(&path, &two_meta, &generation_id).expect("writer output proves");
        mutate_line(&path, 3, |o| {
            o.insert(
                ROLLOUT_GENERATION_ID_FIELD.into(),
                uuid::Uuid::new_v4().to_string().into(),
            );
        });
        let proof = proves_new_generation(&path, &two_meta, &generation_id).expect_err("split id");
        assert!(
            proof.contains("differs from the expected generation"),
            "{proof}"
        );
    }
}

// ── M2: prior-generation realtime eligibility ─────────────────────────────

fn realtime_line(ordinal: u64, id: &str) -> String {
    serde_json::to_string(&RolloutLine {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        ordinal: Some(ordinal),
        item: RolloutItem::RealtimeItem(codex_protocol::realtime::RealtimeItem {
            id: id.into(),
            realtime_session_id: "rt-session".into(),
            content: codex_protocol::realtime::RealtimeItemContent::RealtimeSessionStarted,
        }),
    })
    .expect("encode realtime line")
}

fn session_meta_line(subagent_history_start_ordinal: Option<u64>) -> String {
    serde_json::to_string(&RolloutLine {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        ordinal: Some(0),
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                history_mode: ThreadHistoryMode::Paginated,
                subagent_history_start_ordinal,
                ..SessionMeta::default()
            },
            git: None,
        }),
    })
    .expect("encode session meta line")
}

#[test]
fn parse_prior_realtime_items_returns_every_row_in_order_without_a_boundary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        session_meta_line(None),
        realtime_line(1, "rt-a"),
        realtime_line(2, "rt-b"),
        realtime_line(3, "rt-c"),
    );
    std::fs::write(&path, body).unwrap();

    let ids: Vec<String> = parse_prior_realtime_items(&path)
        .expect("read realtime rows")
        .into_iter()
        .map(|item| item.id)
        .collect();
    assert_eq!(ids, vec!["rt-a", "rt-b", "rt-c"]);
}

#[test]
fn parse_prior_realtime_items_excludes_inherited_subagent_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        session_meta_line(Some(3)),
        realtime_line(1, "rt-inherited-a"),
        realtime_line(2, "rt-inherited-b"),
        realtime_line(3, "rt-child"),
    );
    std::fs::write(&path, body).unwrap();

    let ids: Vec<String> = parse_prior_realtime_items(&path)
        .expect("read realtime rows")
        .into_iter()
        .map(|item| item.id)
        .collect();
    assert_eq!(
        ids,
        vec!["rt-child"],
        "rows below subagent_history_start_ordinal stay inherited history"
    );
}

fn realtime_line_without_ordinal(id: &str) -> String {
    let mut line = serde_json::to_value(RolloutLine {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        ordinal: None,
        item: RolloutItem::RealtimeItem(codex_protocol::realtime::RealtimeItem {
            id: id.into(),
            realtime_session_id: "rt-session".into(),
            content: codex_protocol::realtime::RealtimeItemContent::RealtimeSessionStarted,
        }),
    })
    .expect("encode realtime line");
    line.as_object_mut().expect("object").remove("ordinal");
    serde_json::to_string(&line).expect("encode realtime line without ordinal")
}

#[test]
fn parse_prior_realtime_items_rejects_unprovable_authority_input() {
    struct Case {
        name: &'static str,
        body: String,
        needle: &'static str,
    }
    let cases = [
        Case {
            name: "malformed JSON",
            body: format!(
                "{}\n{{not-json\n{}\n",
                session_meta_line(None),
                realtime_line(1, "rt-a")
            ),
            needle: "malformed JSON",
        },
        Case {
            name: "malformed envelope",
            body: format!(
                "{}\n{{\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"ordinal\":1}}\n{}\n",
                session_meta_line(None),
                realtime_line(1, "rt-a")
            ),
            needle: "malformed rollout envelope",
        },
        Case {
            name: "missing ordinal",
            body: format!(
                "{}\n{}\n{}\n",
                session_meta_line(None),
                realtime_line_without_ordinal("rt-legacy"),
                realtime_line(1, "rt-a"),
            ),
            needle: "missing a paginated ordinal",
        },
        Case {
            name: "missing SessionMeta",
            body: format!("{}\n", realtime_line(1, "rt-a")),
            needle: "without SessionMeta",
        },
        Case {
            name: "ambiguous SessionMeta",
            body: format!(
                "{}\n{}\n{}\n",
                session_meta_line(None),
                session_meta_line(Some(3)),
                realtime_line(3, "rt-child"),
            ),
            needle: "conflicting SessionMeta",
        },
    ];
    for case in cases {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(&path, &case.body).unwrap();
        let err = parse_prior_realtime_items(&path).expect_err(case.name);
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "{}: {err}",
            case.name
        );
        assert!(
            err.to_string().contains(case.needle),
            "{}: expected {:?} in {err}",
            case.name,
            case.needle
        );
    }
}
