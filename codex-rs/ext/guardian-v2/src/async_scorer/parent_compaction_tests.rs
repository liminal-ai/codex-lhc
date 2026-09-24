//! Checkpoint identity and complete-item bounds remain unchanged after policy extraction.

use super::super::config::DEFAULT_PARENT_COMPACTION_TOKENS;
use super::super::config::GuardianV2Config;
use super::*;
use anyhow::Result;
use codex_core::context::GuardianContextMode;
use codex_extension_api::ConversationHistorySnapshot;
use codex_features::GuardianV2ConfigToml;
use codex_history::CompactionCheckpoint;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use pretty_assertions::assert_eq;

#[test]
fn encrypted_parent_compaction_preserves_the_latest_valid_item() {
    let older = ResponseItem::Compaction {
        id: Some(ResponseItemId::from_server("cmp_older".to_owned())),
        encrypted_content: "older encrypted summary".to_owned(),
        internal_chat_message_metadata_passthrough: None,
    };
    let latest = ResponseItem::ContextCompaction {
        id: Some(ResponseItemId::from_server("cmp_latest".to_owned())),
        encrypted_content: Some("latest encrypted summary".to_owned()),
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        encrypted_checkpoint(
            [&older, &latest].into_iter(),
            DEFAULT_PARENT_COMPACTION_TOKENS,
        ),
        Ok(Some(latest.clone()))
    );
    assert_eq!(
        encrypted_checkpoint(
            [&latest, &older].into_iter(),
            DEFAULT_PARENT_COMPACTION_TOKENS,
        ),
        Ok(Some(older))
    );
}

#[test]
fn encrypted_parent_compaction_rejects_invalid_latest_item() {
    let older = ResponseItem::Compaction {
        id: Some(ResponseItemId::from_server("cmp_older".to_owned())),
        encrypted_content: "older encrypted summary".to_owned(),
        internal_chat_message_metadata_passthrough: None,
    };
    let invalid = [
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "encrypted summary without an ID".to_owned(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Compaction {
            id: Some(ResponseItemId::from_server("cmp_empty".to_owned())),
            encrypted_content: String::new(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ContextCompaction {
            id: None,
            encrypted_content: Some("encrypted context without an ID".to_owned()),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ContextCompaction {
            id: Some(ResponseItemId::from_server("cmp_missing".to_owned())),
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ContextCompaction {
            id: Some(ResponseItemId::from_server("cmp_empty".to_owned())),
            encrypted_content: Some(String::new()),
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    for latest in &invalid {
        assert_eq!(
            encrypted_checkpoint(
                [&older, latest].into_iter(),
                DEFAULT_PARENT_COMPACTION_TOKENS,
            ),
            Err(ParentCompactionError::Unusable),
            "an unusable latest summary must not resurrect older context"
        );
    }
}

#[test]
fn encrypted_parent_compaction_rejects_oversized_latest_item() -> Result<()> {
    let max_compaction_bytes =
        TruncationPolicy::Tokens(DEFAULT_PARENT_COMPACTION_TOKENS).byte_budget();
    let mut bounded = [
        ResponseItem::Compaction {
            id: Some(ResponseItemId::from_server("cmp_bounded".to_owned())),
            encrypted_content: String::new(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ContextCompaction {
            id: Some(ResponseItemId::from_server("ctx_bounded".to_owned())),
            encrypted_content: Some(String::new()),
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    for item in &mut bounded {
        let envelope_bytes = serde_json::to_vec(&*item)?.len();
        let encrypted_content = match item {
            ResponseItem::Compaction {
                encrypted_content, ..
            }
            | ResponseItem::ContextCompaction {
                encrypted_content: Some(encrypted_content),
                ..
            } => encrypted_content,
            _ => unreachable!("test fixtures are encrypted compaction items"),
        };
        *encrypted_content = "a".repeat(max_compaction_bytes - envelope_bytes);
        assert_eq!(serde_json::to_vec(&*item)?.len(), max_compaction_bytes);
        assert_eq!(
            encrypted_checkpoint(std::iter::once(&*item), DEFAULT_PARENT_COMPACTION_TOKENS,),
            Ok(Some(item.clone()))
        );

        let mut oversized = item.clone();
        match &mut oversized {
            ResponseItem::Compaction {
                encrypted_content, ..
            }
            | ResponseItem::ContextCompaction {
                encrypted_content: Some(encrypted_content),
                ..
            } => encrypted_content.push('a'),
            _ => unreachable!("test fixtures are encrypted compaction items"),
        }
        assert_eq!(
            serde_json::to_vec(&oversized)?.len(),
            max_compaction_bytes + 1
        );
        assert_eq!(
            encrypted_checkpoint(
                [&*item, &oversized].into_iter(),
                DEFAULT_PARENT_COMPACTION_TOKENS,
            ),
            Err(ParentCompactionError::Oversized),
            "an oversized latest summary must not resurrect older context"
        );
    }

    let oversized_metadata = ResponseItem::ContextCompaction {
        id: Some(ResponseItemId::from_server(
            "ctx_oversized_metadata".to_owned(),
        )),
        encrypted_content: Some("bounded encrypted summary".to_owned()),
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("a".repeat(max_compaction_bytes)),
            ..Default::default()
        }),
    };
    assert!(serde_json::to_vec(&oversized_metadata)?.len() > max_compaction_bytes);
    assert_eq!(
        encrypted_checkpoint(
            [&bounded[0], &oversized_metadata].into_iter(),
            DEFAULT_PARENT_COMPACTION_TOKENS,
        ),
        Err(ParentCompactionError::Oversized),
        "oversized passthrough metadata must not bypass the complete-item limit"
    );

    Ok(())
}

struct LhcFoldedHistory(Vec<ResponseItem>);

impl ConversationHistorySnapshot for LhcFoldedHistory {
    fn history_version(&self) -> u64 {
        1
    }

    fn user_message_revision(&self) -> u64 {
        1
    }

    fn items(&self) -> Box<dyn Iterator<Item = &ResponseItem> + Send + '_> {
        Box::new(self.0.iter())
    }
}

fn lhc_folded_history() -> LhcFoldedHistory {
    LhcFoldedHistory(vec![ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: "folded LHC band: do not publish the private report.".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }])
}

fn guardian_config(reuse_parent_compaction: bool) -> GuardianV2Config {
    GuardianV2Config::from_overrides(GuardianV2ConfigToml {
        reuse_parent_compaction: Some(reuse_parent_compaction),
        ..Default::default()
    })
    .expect("guardian v2 config")
}

#[test]
fn lhc_folded_history_omits_native_parent_compaction() {
    let history = lhc_folded_history();
    let config = guardian_config(/*reuse_parent_compaction*/ true);
    for mode in [
        GuardianContextMode::Legacy,
        GuardianContextMode::ThreadOwned,
    ] {
        let selected =
            select_parent_compaction(mode, &config, &history, |_| true, Some("matching"))
                .expect("LHC fold without a native checkpoint is a valid omit");
        assert_eq!(selected.item, None, "{mode:?}");
        assert!(history.latest_compaction().is_none());
    }
}

#[test]
fn lhc_folded_thread_owned_history_does_not_require_sync_without_a_checkpoint() {
    let history = lhc_folded_history();
    let config = guardian_config(/*reuse_parent_compaction*/ false);
    let selected = select_parent_compaction(
        GuardianContextMode::ThreadOwned,
        &config,
        &history,
        |_| false,
        /*legacy_model_hash*/ None,
    )
    .expect("missing LHC native checkpoint must not fail closed");
    assert_eq!(
        selected,
        ParentCompaction {
            item: None,
            model_hash: None,
        }
    );
}

fn encrypted_checkpoint<'a>(
    items: impl Iterator<Item = &'a ResponseItem>,
    max_parent_compaction_tokens: usize,
) -> Result<Option<ResponseItem>, ParentCompactionError> {
    let items = items
        .cloned()
        .map(ResponseItemEnvelope::from)
        .collect::<Vec<_>>();
    encrypted_parent_compaction(
        CompactionCheckpoint::latest(&items),
        max_parent_compaction_tokens,
    )
}
