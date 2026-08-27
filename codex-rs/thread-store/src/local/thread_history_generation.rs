use std::io::SeekFrom;
use std::path::Path;

use codex_history::ROLLOUT_GENERATION_ID_FIELD;
use codex_history::RolloutOrdinalState;
use codex_protocol::protocol::ThreadHistoryMode;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::BufReader;

use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const DURABLE_GENERATION_ID_TAG: u8 = 1;
const LEGACY_HEAD_DIGEST_TAG: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RolloutGenerationId {
    Durable([u8; 36]),
    LegacyHeadDigest([u8; 32]),
}

impl RolloutGenerationId {
    pub fn encode(self) -> Vec<u8> {
        match self {
            Self::Durable(generation_id) => {
                let mut encoded = Vec::with_capacity(generation_id.len() + 1);
                encoded.push(DURABLE_GENERATION_ID_TAG);
                encoded.extend(generation_id);
                encoded
            }
            Self::LegacyHeadDigest(digest) => {
                let mut encoded = Vec::with_capacity(digest.len() + 1);
                encoded.push(LEGACY_HEAD_DIGEST_TAG);
                encoded.extend(digest);
                encoded
            }
        }
    }

    pub fn decode(encoded: &[u8]) -> Option<Self> {
        match encoded {
            [DURABLE_GENERATION_ID_TAG, generation_id @ ..] => {
                Some(Self::Durable(generation_id.try_into().ok()?))
            }
            [LEGACY_HEAD_DIGEST_TAG, digest @ ..] => {
                Some(Self::LegacyHeadDigest(digest.try_into().ok()?))
            }
            [] | [_, ..] => None,
        }
    }
}

pub(super) fn rollout_generation_id(
    value: &serde_json::Value,
    persisted_line: &[u8],
) -> ThreadStoreResult<RolloutGenerationId> {
    match value.get(ROLLOUT_GENERATION_ID_FIELD) {
        Some(serde_json::Value::String(generation_id)) => Ok(RolloutGenerationId::Durable(
            generation_id
                .as_bytes()
                .try_into()
                .map_err(|_| ThreadStoreError::Internal {
                    message: "durable rollout has an invalid generation ID".to_string(),
                })?,
        )),
        Some(_) => Err(ThreadStoreError::Internal {
            message: "durable rollout has a non-string generation ID".to_string(),
        }),
        None => Ok(RolloutGenerationId::LegacyHeadDigest(
            Sha256::digest(persisted_line).into(),
        )),
    }
}

pub(super) struct RolloutHead {
    pub initial_ordinal: u64,
    pub rollout_generation_id: RolloutGenerationId,
    pub subagent_history_start_ordinal: Option<u64>,
}

pub(super) async fn read_rollout_head(
    reader: &mut BufReader<tokio::fs::File>,
    rollout_path: &Path,
) -> ThreadStoreResult<RolloutHead> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(thread_store_io_error)?
            == 0
        {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value = match serde_json::from_slice::<serde_json::Value>(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let rollout_generation_id = rollout_generation_id(&value, &line);
        let rollout_line = match codex_rollout::decode_rollout_line(value) {
            Ok(rollout_line) => rollout_line,
            Err(_) => continue,
        };
        match rollout_line.item {
            codex_rollout::RolloutItem::SessionMeta(session_meta) => {
                let ordinal_state = match session_meta.meta.history_mode {
                    ThreadHistoryMode::Legacy => RolloutOrdinalState::Legacy,
                    ThreadHistoryMode::Paginated => RolloutOrdinalState::Paginated {
                        next: rollout_line.ordinal,
                    },
                };
                let initial_ordinal = ordinal_state
                    .current()
                    .map_err(thread_store_io_error)?
                    .ok_or_else(|| ThreadStoreError::Internal {
                        message: format!(
                            "thread history projection for {} is not paginated",
                            session_meta.meta.id
                        ),
                    })?;
                return Ok(RolloutHead {
                    initial_ordinal,
                    rollout_generation_id: rollout_generation_id?,
                    subagent_history_start_ordinal: session_meta
                        .meta
                        .subagent_history_start_ordinal,
                });
            }
            codex_rollout::RolloutItem::ResponseItem(_)
            | codex_rollout::RolloutItem::InterAgentCommunication(_) => {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "rollout at {} does not start with session metadata",
                        rollout_path.display()
                    ),
                });
            }
            codex_rollout::RolloutItem::InterAgentCommunicationMetadata { .. }
            | codex_rollout::RolloutItem::Compacted(_)
            | codex_rollout::RolloutItem::TurnContext(_)
            | codex_rollout::RolloutItem::WorldState(_)
            | codex_rollout::RolloutItem::SecurityRiskScore(_)
            | codex_rollout::RolloutItem::RealtimeItem(_)
            | codex_rollout::RolloutItem::EventMsg(_) => {}
        }
    }
    Err(ThreadStoreError::Internal {
        message: format!("rollout at {} is empty", rollout_path.display()),
    })
}

pub(super) async fn projection_frontier_matches(
    file: &mut BufReader<tokio::fs::File>,
    file_end_offset: u64,
    next_byte_offset: u64,
    next_ordinal: u64,
    initial_ordinal: u64,
) -> ThreadStoreResult<bool> {
    if next_byte_offset == 0 {
        return Ok(next_ordinal == initial_ordinal);
    }
    if next_byte_offset > file_end_offset || next_ordinal <= initial_ordinal {
        return Ok(false);
    }
    let previous_line = previous_nonblank_line(file, next_byte_offset).await?;
    let previous_ordinal = previous_line
        .as_deref()
        .and_then(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .and_then(|value| value.get("ordinal").and_then(serde_json::Value::as_u64));
    Ok(previous_ordinal == next_ordinal.checked_sub(1))
}

async fn previous_nonblank_line(
    file: &mut BufReader<tokio::fs::File>,
    mut end_offset: u64,
) -> ThreadStoreResult<Option<Vec<u8>>> {
    while end_offset > 0 {
        let (line, line_start_offset) = previous_line(file, end_offset).await?;
        if !line.iter().all(u8::is_ascii_whitespace) {
            return Ok(Some(line));
        }
        end_offset = line_start_offset;
    }
    Ok(None)
}

async fn previous_line(
    file: &mut BufReader<tokio::fs::File>,
    end_offset: u64,
) -> ThreadStoreResult<(Vec<u8>, u64)> {
    const READ_CHUNK_SIZE: u64 = 8192;

    let mut trailing_byte = [0];
    file.seek(SeekFrom::Start(end_offset - 1))
        .await
        .map_err(thread_store_io_error)?;
    file.read_exact(&mut trailing_byte)
        .await
        .map_err(thread_store_io_error)?;
    if trailing_byte[0] != b'\n' {
        return Ok((Vec::new(), 0));
    }

    let mut scan_end = end_offset - 1;
    let mut reverse_chunks = Vec::new();
    loop {
        let scan_start = scan_end.saturating_sub(READ_CHUNK_SIZE);
        let chunk_len =
            usize::try_from(scan_end - scan_start).map_err(|_| ThreadStoreError::Internal {
                message: "durable rollout line exceeds addressable memory".to_string(),
            })?;
        let mut chunk = vec![0; chunk_len];
        file.seek(SeekFrom::Start(scan_start))
            .await
            .map_err(thread_store_io_error)?;
        file.read_exact(chunk.as_mut_slice())
            .await
            .map_err(thread_store_io_error)?;
        if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
            reverse_chunks.push(chunk[index + 1..].to_vec());
            let line_start_offset = scan_start
                .checked_add(
                    u64::try_from(index + 1).map_err(|_| ThreadStoreError::Internal {
                        message: "durable rollout byte offset overflow".to_string(),
                    })?,
                )
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "durable rollout byte offset overflow".to_string(),
                })?;
            let line_len = reverse_chunks.iter().map(Vec::len).sum();
            let mut line = Vec::with_capacity(line_len);
            for chunk in reverse_chunks.into_iter().rev() {
                line.extend(chunk);
            }
            return Ok((line, line_start_offset));
        }
        reverse_chunks.push(chunk);
        if scan_start == 0 {
            let line_len = reverse_chunks.iter().map(Vec::len).sum();
            let mut line = Vec::with_capacity(line_len);
            for chunk in reverse_chunks.into_iter().rev() {
                line.extend(chunk);
            }
            return Ok((line, 0));
        }
        scan_end = scan_start;
    }
}

fn thread_store_io_error(err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: err.to_string(),
    }
}
