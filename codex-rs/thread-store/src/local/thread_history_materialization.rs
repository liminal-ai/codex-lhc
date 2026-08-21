use std::io::SeekFrom;
use std::path::Path;

use chrono::DateTime;
use codex_app_server_protocol::ThreadHistoryChangeSet;
use codex_app_server_protocol::project_rollout_line;
use codex_history::RolloutOrdinalState;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::BufReader;
use tracing::warn;

use super::LocalThreadStore;
use super::thread_history::ProjectedRolloutLine;
use super::thread_history::RolloutProjectionStep;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn materialize_to_sqlite(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<()> {
    if store.state_db.is_none() {
        return Ok(());
    }
    let mut projection_state = super::thread_history::projection_state(store, thread_id).await?;
    if projection_state.is_none()
        && !tokio::fs::try_exists(rollout_path)
            .await
            .map_err(thread_store_io_error)?
    {
        return Ok(());
    }
    loop {
        let (session_meta, persisted_initial_ordinal) =
            read_session_meta_rollout_line(rollout_path).await?;
        let ordinal_state = match session_meta.history_mode {
            ThreadHistoryMode::Legacy => RolloutOrdinalState::Legacy,
            ThreadHistoryMode::Paginated => RolloutOrdinalState::Paginated {
                next: persisted_initial_ordinal,
            },
        };
        let initial_ordinal = ordinal_state
            .current()
            .map_err(thread_store_io_error)?
            .ok_or_else(|| ThreadStoreError::Internal {
                message: format!("thread history projection for {thread_id} is not paginated"),
            })?;
        let subagent_history_start_ordinal = session_meta.subagent_history_start_ordinal;
        let start_offset = projection_state.map_or(0, |state| state.next_byte_offset);
        let expected_ordinal = projection_state.map_or(initial_ordinal, |state| state.next_ordinal);
        let projection_read = read_projection_steps(
            rollout_path,
            start_offset,
            expected_ordinal,
            initial_ordinal,
            thread_id,
            subagent_history_start_ordinal,
        )
        .await?;
        let ProjectionRead::Steps {
            projections,
            next_offset,
        } = projection_read
        else {
            let Some(state) = projection_state else {
                continue;
            };
            if super::thread_history::reset_projection(store, thread_id, state).await? {
                let active_file_size = tokio::fs::metadata(rollout_path)
                    .await
                    .map_err(thread_store_io_error)?
                    .len();
                warn!(
                    thread_id = %thread_id,
                    rollout_path = %rollout_path.display(),
                    prior_next_byte_offset = state.next_byte_offset,
                    prior_next_ordinal = state.next_ordinal,
                    active_file_size,
                    replacement_initial_ordinal = initial_ordinal,
                    "resetting thread history projection after durable rollout generation swap"
                );
                projection_state = None;
            } else {
                projection_state =
                    super::thread_history::projection_state(store, thread_id).await?;
            }
            continue;
        };
        // Empty valid records can still consume bytes through blank complete lines.
        if projections.is_empty() && start_offset == next_offset {
            return Ok(());
        }
        return super::thread_history::apply_projection(
            store,
            thread_id,
            start_offset,
            next_offset,
            initial_ordinal,
            projections,
        )
        .await;
    }
}

enum ProjectionRead {
    Steps {
        projections: Vec<RolloutProjectionStep>,
        next_offset: u64,
    },
    GenerationSwap,
}

async fn read_session_meta_rollout_line(
    rollout_path: &Path,
) -> ThreadStoreResult<(codex_protocol::protocol::SessionMeta, Option<u64>)> {
    let file = tokio::fs::File::open(rollout_path)
        .await
        .map_err(thread_store_io_error)?;
    let mut lines = BufReader::new(file).lines();
    while let Some(line) = lines.next_line().await.map_err(thread_store_io_error)? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let rollout_line = match codex_rollout::decode_rollout_line(value) {
            Ok(rollout_line) => rollout_line,
            Err(_) => continue,
        };
        match rollout_line.item {
            codex_rollout::RolloutItem::SessionMeta(session_meta) => {
                return Ok((session_meta.meta, rollout_line.ordinal));
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
            | codex_rollout::RolloutItem::EventMsg(_) => {}
        }
    }
    Err(ThreadStoreError::Internal {
        message: format!("rollout at {} is empty", rollout_path.display()),
    })
}

async fn projection_frontier_matches(
    file: &mut tokio::fs::File,
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
    file: &mut tokio::fs::File,
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
    file: &mut tokio::fs::File,
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

async fn read_projection_steps(
    rollout_path: &Path,
    start_offset: u64,
    expected_ordinal: u64,
    initial_ordinal: u64,
    thread_id: ThreadId,
    subagent_history_start_ordinal: Option<u64>,
) -> ThreadStoreResult<ProjectionRead> {
    let mut file = match tokio::fs::File::open(rollout_path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && start_offset == 0 => {
            return Ok(ProjectionRead::Steps {
                projections: Vec::new(),
                next_offset: 0,
            });
        }
        Err(err) => return Err(thread_store_io_error(err)),
    };
    let file_end_offset = file.metadata().await.map_err(thread_store_io_error)?.len();
    if !projection_frontier_matches(
        &mut file,
        file_end_offset,
        start_offset,
        expected_ordinal,
        initial_ordinal,
    )
    .await?
    {
        return Ok(ProjectionRead::GenerationSwap);
    }
    let byte_count = file_end_offset - start_offset;
    let byte_count = usize::try_from(byte_count).map_err(|_| ThreadStoreError::Internal {
        message: "durable rollout append exceeds addressable memory".to_string(),
    })?;
    let mut bytes = vec![0; byte_count];
    file.seek(SeekFrom::Start(start_offset))
        .await
        .map_err(thread_store_io_error)?;
    file.read_exact(bytes.as_mut_slice())
        .await
        .map_err(thread_store_io_error)?;
    // Only project the newline-terminated prefix; leave a trailing partial record for the next
    // pass.
    let complete_byte_count = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut projections = Vec::new();
    let mut next_ordinal = expected_ordinal;
    let mut next_offset = start_offset;
    let mut pending_rejected_line_count = 0;
    let mut line_start_offset = start_offset;
    // Keep rejected lines pending until a later valid ordinal proves whether they consumed history.
    // This lets a same-ordinal retry replace a failed write without advancing only one checkpoint.
    for line_bytes in bytes[..complete_byte_count].split_inclusive(|byte| *byte == b'\n') {
        let line_end_offset = line_start_offset
            .checked_add(u64::try_from(line_bytes.len()).map_err(|_| {
                ThreadStoreError::Internal {
                    message: "durable rollout byte offset overflow".to_string(),
                }
            })?)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "durable rollout byte offset overflow".to_string(),
            })?;
        if line_bytes.iter().all(u8::is_ascii_whitespace) {
            if pending_rejected_line_count == 0 {
                next_offset = line_end_offset;
            }
            line_start_offset = line_end_offset;
            continue;
        }
        let value = match serde_json::from_slice::<serde_json::Value>(line_bytes) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    thread_id = %thread_id,
                    rollout_path = %rollout_path.display(),
                    line_start_byte_offset = line_start_offset,
                    line_end_byte_offset = line_end_offset,
                    expected_ordinal = next_ordinal,
                    error = %err,
                    "deferring rejected rollout line until a later ordinal resolves it"
                );
                pending_rejected_line_count += 1;
                line_start_offset = line_end_offset;
                continue;
            }
        };
        let value_ordinal = value.get("ordinal").and_then(serde_json::Value::as_u64);
        let line = match codex_rollout::decode_rollout_line(value) {
            Ok(line) => Some(line),
            Err(err) => {
                warn!(
                    thread_id = %thread_id,
                    rollout_path = %rollout_path.display(),
                    line_start_byte_offset = line_start_offset,
                    line_end_byte_offset = line_end_offset,
                    expected_ordinal = next_ordinal,
                    line_ordinal = ?value_ordinal,
                    error = %err,
                    "deferring unknown rollout line until a later ordinal resolves it"
                );
                None
            }
        };
        let ordinal = match line
            .as_ref()
            .and_then(|line| line.ordinal)
            .or(value_ordinal)
        {
            Some(ordinal) => ordinal,
            None if line.is_none() => {
                pending_rejected_line_count += 1;
                line_start_offset = line_end_offset;
                continue;
            }
            None => {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "paginated rollout line for {thread_id} is missing an ordinal"
                    ),
                });
            }
        };
        if ordinal < next_ordinal {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "thread history projection for {thread_id} expected ordinal {next_ordinal}, got {ordinal}"
                ),
            });
        }
        let Some(line) = line else {
            pending_rejected_line_count += 1;
            line_start_offset = line_end_offset;
            continue;
        };
        let skipped_ordinal_count = ordinal - next_ordinal;
        if skipped_ordinal_count > pending_rejected_line_count {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "thread history projection for {thread_id} expected ordinal {next_ordinal}, got {ordinal}; {pending_rejected_line_count} rejected rollout lines cannot cover that gap"
                ),
            });
        }
        let changes = if subagent_history_start_ordinal.is_some_and(|start| ordinal < start) {
            ThreadHistoryChangeSet::default()
        } else {
            project_rollout_line(&line)
        };
        let fallback_created_at_ms = if changes
            .changed_items
            .iter()
            .any(|item| item.started_at_ms.is_none())
        {
            match DateTime::parse_from_rfc3339(line.timestamp.as_str()) {
                Ok(timestamp) => Some(timestamp.timestamp_millis()),
                Err(err) => {
                    warn!(
                        thread_id = %thread_id,
                        rollout_path = %rollout_path.display(),
                        line_start_byte_offset = line_start_offset,
                        line_end_byte_offset = line_end_offset,
                        expected_ordinal = next_ordinal,
                        line_ordinal = ordinal,
                        error = %err,
                        "deferring rollout line with invalid timestamp until a later ordinal resolves it"
                    );
                    pending_rejected_line_count += 1;
                    line_start_offset = line_end_offset;
                    continue;
                }
            }
        } else {
            None
        };
        if skipped_ordinal_count > 0 {
            warn!(
                thread_id = %thread_id,
                rollout_path = %rollout_path.display(),
                line_start_byte_offset = line_start_offset,
                line_end_byte_offset = line_end_offset,
                expected_ordinal = next_ordinal,
                line_ordinal = ordinal,
                skipped_ordinal_start = next_ordinal,
                skipped_ordinal_end_exclusive = ordinal,
                "skipping rollout ordinal range after rejected lines"
            );
            projections.push(RolloutProjectionStep::SkippedOrdinalRange {
                start_ordinal: next_ordinal,
                end_ordinal_exclusive: ordinal,
            });
        }
        pending_rejected_line_count = 0;
        let next_line_ordinal =
            ordinal
                .checked_add(1)
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "rollout ordinal exceeds SQLite integer range".to_string(),
                })?;
        projections.push(RolloutProjectionStep::Line(ProjectedRolloutLine {
            ordinal,
            start_byte_offset: line_start_offset,
            end_byte_offset: line_end_offset,
            fallback_created_at_ms,
            changes,
        }));
        next_ordinal = next_line_ordinal;
        next_offset = line_end_offset;
        line_start_offset = line_end_offset;
    }
    Ok(ProjectionRead::Steps {
        projections,
        next_offset,
    })
}

fn thread_store_io_error(err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: err.to_string(),
    }
}

#[cfg(test)]
#[path = "thread_history_materialization_tests.rs"]
mod tests;
