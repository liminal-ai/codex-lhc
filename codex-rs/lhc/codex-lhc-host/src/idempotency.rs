//! Stable idempotency keys for LHC capture under Codex.
//!
//! # Key shape (restart-stable)
//!
//! Prefer a host-stable `ResponseItemId` when present, **plus content digest**:
//! ```text
//! codex:{thread}:id:{item_id}:{digest}:{event_kind}[:{part}]
//! ```
//! The digest is required so status-advancing items (e.g. ImageGenerationCall
//! `in_progress` → `completed`) do not collide and keep a stale body (H5).
//! Same id + same content on restart/replay still mint the same key.
//!
//! Fallback when the item has no id (unreachable for host-assigned variants;
//! kept as a defensive path for CompactionTrigger/Other which map to nothing):
//! ```text
//! codex:{thread}:anon:{digest}:{occurrence}:{event_kind}[:{part}]
//! ```
//!
//! Turn boundaries:
//! ```text
//! codex:{thread}:turn_end:{turn_id}:{reason}
//! ```
//!
//! Model / thinking-level changes — **transition keys** (restart-stable):
//! ```text
//! codex:{thread}:model_change:{previous}:{new}
//! codex:{thread}:thinking_level_change:{previous}:{new}
//! ```
//! Same prev→new re-fire (resume) collides and LHC skips. A genuine toggle
//! `a→b→a→b` reuses the first `a→b` key (accepted trade; document H7).
//!
//! # Why id-primary (F1)
//!
//! Two requirements:
//! 1. The **same** logical item re-presented after restart/replay/retry must
//!    mint the **same** key so LHC `DuplicateIdempotencyKey` absorbs it.
//! 2. Two **genuinely distinct** occurrences of identical content must mint
//!    **different** keys.
//!
//! High-water occurrence seeding alone satisfies only (2) and breaks (1):
//! reopen advances past stored occ, so the same item re-keys as occ+1.
//!
//! The host assigns a durable `ResponseItemId` at the history boundary
//! (`Session::assign_missing_response_item_ids`). That id is the typed
//! discriminator for (1). Distinct live occurrences get distinct ids from the
//! host, so (2) holds without a counter heuristic on the id path.
//!
//! Occurrence counters still apply on the anonymous fallback path and are
//! resolved lazily from stored `anon:` keys only when `item_stable_id` is
//! `None`. Normal ID-bearing open starts the tracker empty.
//!
//! `thread` is percent-escaped (`:` → `%3A`, `%` → `%25`).

use std::collections::HashMap;
use std::collections::HashSet;

use codex_protocol::models::ResponseItem;
use sha2::Digest;
use sha2::Sha256;
use tracing::warn;

/// Running occurrence counter keyed by item digest (anonymous path only).
#[derive(Debug, Default, Clone)]
pub struct OccurrenceTracker {
    counts: HashMap<String, u64>,
    /// Digests whose archive high-water has already been loaded this process.
    resolved: HashSet<String>,
}

impl OccurrenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next(&mut self, digest: &str) -> u64 {
        let entry = self.counts.entry(digest.to_string()).or_insert(0);
        let occ = *entry;
        *entry += 1;
        occ
    }

    pub fn merge_monotonic(&mut self, other: &OccurrenceTracker) {
        for (digest, &other_next) in &other.counts {
            let entry = self.counts.entry(digest.clone()).or_insert(0);
            *entry = (*entry).max(other_next);
        }
        self.resolved.extend(other.resolved.iter().cloned());
    }

    pub fn observe(&mut self, digest: &str, occ: u64) {
        let entry = self.counts.entry(digest.to_string()).or_insert(0);
        *entry = (*entry).max(occ.saturating_add(1));
    }

    pub fn is_resolved(&self, digest: &str) -> bool {
        self.resolved.contains(digest)
    }

    pub fn mark_resolved(&mut self, digest: &str) {
        self.resolved.insert(digest.to_string());
    }
}

/// Escape `:` and `%` in thread ids so key structure stays parseable.
pub fn encode_thread_id(thread_id: &str) -> String {
    thread_id.replace('%', "%25").replace(':', "%3A")
}

/// SHA-256 hex digest of the canonical JSON encoding of `item`.
pub fn item_digest(item: &ResponseItem) -> String {
    match serde_json::to_vec(item) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hex_encode(&hasher.finalize())
        }
        Err(err) => {
            warn!(
                ?err,
                "LHC: ResponseItem serialize failed; using fallback digest"
            );
            let mut hasher = Sha256::new();
            hasher.update(b"serialize-failed:");
            hasher.update(format!("{item:?}").as_bytes());
            hex_encode(&hasher.finalize())
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let hi = usize::from(b >> 4);
        let lo = usize::from(b & 0xf);
        out.push(HEX[hi] as char);
        out.push(HEX[lo] as char);
    }
    out
}

/// Extract a non-empty host item id when present.
pub fn item_stable_id(item: &ResponseItem) -> Option<String> {
    item.id().and_then(|id| {
        let s = id.as_str();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    })
}

/// Build a capture idempotency key for a mapped sub-event.
///
/// When `stable_id` is `Some`, the key is id+content-digest (restart-stable
/// for identical content; distinct for status mutations of the same id).
/// Otherwise falls back to digest+occurrence (anonymous defensive path).
pub fn item_event_key(
    thread_id: &str,
    stable_id: Option<&str>,
    digest: &str,
    occurrence: u64,
    event_kind: &str,
    part: Option<&str>,
) -> String {
    let tid = encode_thread_id(thread_id);
    match stable_id {
        Some(id) => {
            let iid = encode_thread_id(id);
            // Include full content digest so in_progress→completed does not
            // keep the stale body (H5). Restart of identical content collides.
            match part {
                Some(part) => format!("codex:{tid}:id:{iid}:{digest}:{event_kind}:{part}"),
                None => format!("codex:{tid}:id:{iid}:{digest}:{event_kind}"),
            }
        }
        None => match part {
            Some(part) => {
                format!("codex:{tid}:anon:{digest}:{occurrence}:{event_kind}:{part}")
            }
            None => {
                format!("codex:{tid}:anon:{digest}:{occurrence}:{event_kind}")
            }
        },
    }
}

/// Turn-boundary key (abort/stop/error). Stable for a given turn + reason.
pub fn turn_end_key(thread_id: &str, turn_id: &str, reason: &str) -> String {
    let tid = encode_thread_id(thread_id);
    let turn = encode_thread_id(turn_id);
    format!("codex:{tid}:turn_end:{turn}:{reason}")
}

/// Model-change key from the transition itself (restart-stable on re-fire).
pub fn model_change_key(thread_id: &str, previous: &str, new: &str) -> String {
    let tid = encode_thread_id(thread_id);
    let prev = encode_thread_id(previous);
    let next = encode_thread_id(new);
    format!("codex:{tid}:model_change:{prev}:{next}")
}

/// Thinking-level-change key from the transition itself.
pub fn thinking_level_change_key(thread_id: &str, previous: &str, new: &str) -> String {
    let tid = encode_thread_id(thread_id);
    let prev = encode_thread_id(previous);
    let next = encode_thread_id(new);
    format!("codex:{tid}:thinking_level_change:{prev}:{next}")
}

/// Seed an occurrence tracker from stored LHC idempotency keys.
///
/// Only `anon:` keys contribute to occurrence high-water. Id-primary keys are
/// inherently restart-stable and need no counter.
pub fn seed_occurrence_from_keys<'a>(keys: impl IntoIterator<Item = &'a str>) -> OccurrenceTracker {
    let mut tracker = OccurrenceTracker::new();
    for key in keys {
        if let Some((digest, occ)) = parse_anon_key_digest_occ(key) {
            tracker.observe(&digest, occ);
        }
    }
    tracker
}

fn parse_anon_key_digest_occ(key: &str) -> Option<(String, u64)> {
    // codex:{tid}:anon:{digest}:{occ}:{event_kind}[:part]
    let rest = key.strip_prefix("codex:")?;
    let after_tid = rest.split_once(':')?.1;
    let after_anon = after_tid.strip_prefix("anon:")?;
    let mut parts = after_anon.splitn(3, ':');
    let digest = parts.next()?;
    let occ_str = parts.next()?;
    let _kind = parts.next()?;
    let occ: u64 = occ_str.parse().ok()?;
    if digest.is_empty() {
        return None;
    }
    Some((digest.to_string(), occ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ResponseItemId;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn user_message_with_id(text: &str, id: &str) -> ResponseItem {
        ResponseItem::Message {
            id: Some(ResponseItemId::from_server(id.to_string())),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[test]
    fn same_item_id_mints_identical_key_on_replay() {
        let item = user_message_with_id("hello", "msg_stable_1");
        let d = item_digest(&item);
        let sid = item_stable_id(&item);
        let k0 = item_event_key("t", sid.as_deref(), &d, 0, "user_prompt", None);
        let k1 = item_event_key("t", sid.as_deref(), &d, 99, "user_prompt", None);
        assert_eq!(
            k0, k1,
            "id-primary keys ignore occurrence — restart re-presentation collides"
        );
        assert!(k0.contains(":id:msg_stable_1:"), "{k0}");
    }

    #[test]
    fn distinct_item_ids_do_not_collide() {
        let a = user_message_with_id("hello", "msg_a");
        let b = user_message_with_id("hello", "msg_b");
        let da = item_digest(&a);
        let db = item_digest(&b);
        let ka = item_event_key(
            "t",
            item_stable_id(&a).as_deref(),
            &da,
            0,
            "user_prompt",
            None,
        );
        let kb = item_event_key(
            "t",
            item_stable_id(&b).as_deref(),
            &db,
            0,
            "user_prompt",
            None,
        );
        assert_ne!(ka, kb);
    }

    #[test]
    fn anonymous_items_get_distinct_occurrence_keys() {
        let mut tracker = OccurrenceTracker::new();
        let item = user_message("hello");
        let d = item_digest(&item);
        let o0 = tracker.next(&d);
        let o1 = tracker.next(&d);
        let k0 = item_event_key("t1", None, &d, o0, "user_prompt", None);
        let k1 = item_event_key("t1", None, &d, o1, "user_prompt", None);
        assert_ne!(k0, k1);
        assert!(k0.contains(":anon:"), "{k0}");
    }

    #[test]
    fn seed_from_anon_keys_raises_high_water() {
        let d = "deadbeef";
        let k0 = item_event_key("s", None, d, 0, "user_prompt", None);
        let k1 = item_event_key("s", None, d, 1, "user_prompt", None);
        let mut t = seed_occurrence_from_keys([k0.as_str(), k1.as_str()]);
        assert_eq!(t.next(d), 2);
    }

    #[test]
    fn seed_ignores_id_primary_keys() {
        let k = item_event_key("s", Some("msg_1"), "unused", 0, "user_prompt", None);
        let t = seed_occurrence_from_keys([k.as_str()]);
        // empty tracker
        let mut t2 = t;
        assert_eq!(t2.next("any"), 0);
    }

    #[test]
    fn turn_end_key_stable() {
        let a = turn_end_key("t", "turn-1", "stop");
        let b = turn_end_key("t", "turn-1", "stop");
        let c = turn_end_key("t", "turn-1", "abort");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn model_change_key_is_transition_stable() {
        let a = model_change_key("s", "gpt-a", "gpt-b");
        let b = model_change_key("s", "gpt-a", "gpt-b");
        let c = model_change_key("s", "gpt-b", "gpt-c");
        assert_eq!(a, b, "same prev→new must collide (restart re-fire)");
        assert_ne!(a, c);
        assert!(a.contains(":model_change:gpt-a:gpt-b"), "{a}");
    }

    #[test]
    fn thinking_level_change_key_is_transition_stable() {
        let a = thinking_level_change_key("s", "low", "high");
        let b = thinking_level_change_key("s", "low", "high");
        assert_eq!(a, b);
    }

    /// Fallback digest (serialize-failed path) must be stable for the same
    /// Debug form — no process-local pointers. We cannot force `ResponseItem`
    /// serde to fail, so assert the normal digest path is content-stable
    /// (already the production path for every real item).
    #[test]
    fn item_digest_is_content_stable_not_pointer_based() {
        let a = user_message("stable digest body");
        let b = user_message("stable digest body");
        let d0 = item_digest(&a);
        let d1 = item_digest(&b);
        assert_eq!(d0, d1);
        assert_eq!(d0.len(), 64, "sha256 hex");
        assert!(
            d0.chars().all(|c| c.is_ascii_hexdigit()),
            "digest must be pure hex, not a pointer dump"
        );
    }
}
