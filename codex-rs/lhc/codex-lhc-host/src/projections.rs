//! Caller-bounded LHC projections used by capture open, coverage, and reconcile.
//!
//! `list_events` remains the explicit full-archive read. Normal startup paths
//! use `thread_frontier`, `event_key_prefix_counts`, and
//! `list_event_keys_by_prefix`.

use std::collections::HashMap;
use std::fmt;

use codex_protocol::models::ResponseItem;
use lhc::intake_stream::EventKeyPageQuery;
use lhc::intake_stream::EventKeyReference;
use tracing::warn;

use crate::compact_bridge::COMPACT_MARKER_KEY_SEGMENT;
use crate::compact_bridge::CoverageClass;
use crate::compact_bridge::DerivedProvenance;
use crate::compact_bridge::classify_coverage;
use crate::compact_bridge::content_identity_digest;
use crate::idempotency::OccurrenceTracker;
use crate::idempotency::encode_thread_id;
use crate::idempotency::item_digest;
use crate::idempotency::item_stable_id;
use crate::idempotency::seed_occurrence_from_keys;
use crate::session::LhcSession;

/// Algorithmic counters for selected columns/rows on bounded vs full-archive reads.
///
/// These are invocation/result counts, not timings. `payload_parses` is 1 per
/// `list_events` row (the SDK parses every payload JSON); projection APIs
/// never increment it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectionQueryStats {
    pub list_events_calls: u64,
    pub list_events_rows: u64,
    pub payload_parses: u64,
    pub frontier_calls: u64,
    pub frontier_rows: u64,
    pub prefix_count_calls: u64,
    pub prefix_count_input_prefixes: u64,
    pub prefix_count_rows: u64,
    pub key_list_calls: u64,
    pub key_list_rows: u64,
}

/// Indexed key prefix for a host-stable item id.
pub fn stable_id_key_prefix(thread_id: &str, item_id: &str) -> String {
    let tid = encode_thread_id(thread_id);
    let iid = encode_thread_id(item_id);
    format!("codex:{tid}:id:{iid}:")
}

/// Indexed key prefix for the anonymous digest path.
pub fn anon_digest_key_prefix(thread_id: &str, digest: &str) -> String {
    let tid = encode_thread_id(thread_id);
    format!("codex:{tid}:anon:{digest}:")
}

/// Indexed key prefix for compact-marker bookkeeping keys.
pub fn compact_marker_key_prefix(thread_id: &str) -> String {
    let tid = encode_thread_id(thread_id);
    format!("codex:{tid}:{COMPACT_MARKER_KEY_SEGMENT}:")
}

/// `compact_point` encoded in a compact-marker idempotency key, if present.
///
/// Key shape: `codex:{tid}:compact_marker:{tip}:{compact_point}:{covered_from}:{body_fp}`.
/// `tid` and `tip` are `encode_thread_id`'d, so they contain no raw `:`.
pub fn compact_point_from_marker_key(key: &str) -> Option<i64> {
    let mut parts = key.splitn(6, ':');
    let (Some("codex"), Some(_tid), Some(COMPACT_MARKER_KEY_SEGMENT), Some(_tip), Some(point), _) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    point.parse().ok()
}

/// Visible refusal when a legacy prefix walk hits the SDK total lookup cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyOccurrenceError {
    CapExhausted { prefix: String, looked_up: usize },
    Query(String),
}

impl fmt::Display for LegacyOccurrenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapExhausted { prefix, looked_up } => {
                write!(
                    f,
                    "legacy occurrence prefix walk cap-exhausted after {looked_up} keys under {prefix}; refusing rather than guessing"
                )
            }
            Self::Query(reason) => write!(f, "legacy occurrence prefix walk failed: {reason}"),
        }
    }
}

/// Cursor-stable, hard-capped listing of keys under `prefix`.
///
/// Cap exhaustion is an error: the caller must not treat a truncated walk as
/// the whole prefix.
pub async fn list_keys_under_prefix(
    session: &LhcSession,
    prefix: &str,
) -> Result<Vec<EventKeyReference>, LegacyOccurrenceError> {
    let mut cursor = None;
    let mut out = Vec::new();
    loop {
        let page = session
            .list_event_keys_by_prefix(EventKeyPageQuery {
                prefix: prefix.to_string(),
                cursor,
                limit: None,
            })
            .await
            .map_err(LegacyOccurrenceError::Query)?;
        if page.cap_exhausted {
            return Err(LegacyOccurrenceError::CapExhausted {
                prefix: prefix.to_string(),
                looked_up: out.len().saturating_add(page.keys.len()),
            });
        }
        out.extend(page.keys);
        if page.complete {
            return Ok(out);
        }
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => {
                return Err(LegacyOccurrenceError::CapExhausted {
                    prefix: prefix.to_string(),
                    looked_up: out.len(),
                });
            }
        }
    }
}

/// Seed anonymous occurrence high-water for `item` from stored keys, lazily.
///
/// Activates only when `item_stable_id(item)` is `None`. Id-bearing items
/// leave the tracker empty. Cap exhaustion refuses; it never guesses and
/// never resets already-resolved occurrence state.
pub async fn ensure_legacy_occurrence(
    session: &LhcSession,
    tracker: &mut OccurrenceTracker,
    item: &ResponseItem,
) -> Result<(), LegacyOccurrenceError> {
    if item_stable_id(item).is_some() {
        return Ok(());
    }
    let digest = item_digest(item);
    if tracker.is_resolved(&digest) {
        return Ok(());
    }
    let prefix = anon_digest_key_prefix(&session.thread_id, &digest);
    let keys = match list_keys_under_prefix(session, &prefix).await {
        Ok(keys) => keys,
        Err(err) => {
            warn!(
                thread_id = %session.thread_id,
                %err,
                "LHC: legacy ID-less occurrence listing refused"
            );
            return Err(err);
        }
    };
    let seeded = seed_occurrence_from_keys(keys.iter().map(|k| k.idempotency_key.as_str()));
    tracker.merge_monotonic(&seeded);
    tracker.mark_resolved(&digest);
    Ok(())
}

/// Host items missing from the archive by indexed key-prefix existence/count.
///
/// One `event_key_prefix_counts` call over the caller-supplied host items.
/// Result rows are O(distinct prefixes) = O(host items), never O(archive).
pub async fn host_items_missing_from_archive_indexed(
    session: &LhcSession,
    host_items: &[ResponseItem],
    derived: &DerivedProvenance,
) -> Result<Vec<ResponseItem>, String> {
    let mut prefixes = Vec::new();
    for item in host_items {
        if !is_required(item) {
            continue;
        }
        if let Some(id) = item_stable_id(item) {
            if derived.ids.contains(&id) {
                continue;
            }
            prefixes.push(stable_id_key_prefix(&session.thread_id, &id));
        } else if derived.digests.contains(&content_identity_digest(item)) {
            continue;
        } else {
            prefixes.push(anon_digest_key_prefix(
                &session.thread_id,
                &item_digest(item),
            ));
        }
    }

    let mut remaining: HashMap<String, i64> = HashMap::new();
    if !prefixes.is_empty() {
        let counts = session.event_key_prefix_counts(&prefixes).await?;
        for row in counts {
            remaining.insert(row.prefix, row.count);
        }
    }

    let mut missing = Vec::new();
    for item in host_items {
        if !is_required(item) {
            continue;
        }
        if let Some(id) = item_stable_id(item) {
            if derived.ids.contains(&id) {
                continue;
            }
            let prefix = stable_id_key_prefix(&session.thread_id, &id);
            if remaining.get(&prefix).copied().unwrap_or(0) > 0 {
                continue;
            }
            missing.push(item.clone());
            continue;
        }
        if derived.digests.contains(&content_identity_digest(item)) {
            continue;
        }
        let prefix = anon_digest_key_prefix(&session.thread_id, &item_digest(item));
        let entry = remaining.entry(prefix).or_insert(0);
        if *entry > 0 {
            *entry -= 1;
            continue;
        }
        missing.push(item.clone());
    }
    Ok(missing)
}

fn is_required(item: &ResponseItem) -> bool {
    matches!(classify_coverage(item), CoverageClass::Required)
}

impl LhcSession {
    /// Constant-row durable position. Never reads or parses event payloads.
    pub async fn thread_frontier(&self) -> Result<lhc::intake_stream::ThreadFrontier, String> {
        self.record_stats(|s| s.frontier_calls += 1);
        match self
            .lhc
            .intake_stream
            .thread_frontier(self.thread_ref.clone())
            .await
        {
            lhc::sdk::OpResult::Ok { value } => {
                self.record_stats(|s| s.frontier_rows += 1);
                Ok(value)
            }
            lhc::sdk::OpResult::Err { error } => Err(error.reason),
        }
    }

    /// Caller-bounded existence/count for a finite set of key prefixes.
    pub async fn event_key_prefix_counts(
        &self,
        prefixes: &[String],
    ) -> Result<Vec<lhc::intake_stream::EventKeyPrefixCount>, String> {
        let n = prefixes.len() as u64;
        self.record_stats(|s| {
            s.prefix_count_calls += 1;
            s.prefix_count_input_prefixes += n;
        });
        match self
            .lhc
            .intake_stream
            .event_key_prefix_counts(self.thread_ref.clone(), prefixes)
            .await
        {
            lhc::sdk::OpResult::Ok { value } => {
                let rows = value.len() as u64;
                self.record_stats(|s| s.prefix_count_rows += rows);
                Ok(value)
            }
            lhc::sdk::OpResult::Err { error } => Err(error.reason),
        }
    }

    /// Cursor-stable paginated key listing under one prefix (no payloads).
    pub async fn list_event_keys_by_prefix(
        &self,
        options: EventKeyPageQuery,
    ) -> Result<lhc::intake_stream::EventKeyPage, String> {
        self.record_stats(|s| s.key_list_calls += 1);
        match self
            .lhc
            .intake_stream
            .list_event_keys_by_prefix(self.thread_ref.clone(), options)
            .await
        {
            lhc::sdk::OpResult::Ok { value } => {
                let rows = value.keys.len() as u64;
                self.record_stats(|s| s.key_list_rows += rows);
                Ok(value)
            }
            lhc::sdk::OpResult::Err { error } => Err(error.reason),
        }
    }
}
