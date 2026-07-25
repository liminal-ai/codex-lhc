//! Band-shaped replacement history construction for Chunk 2a eval.
//!
//! Before the compact bridge exists, this builds a **realistic band-shaped
//! history** from a captured LHC event stream by partitioning turns into:
//!
//! - `brief` — oldest third, summarized into one developer note  
//! - `detailed` — middle third, summarized  
//! - `smooth` — near-tail third (excluding full), summarized  
//! - `full` — last few user/assistant turns **verbatim**
//!
//! This is the shape the eventual LHC compact write-back is expected to
//! install via `replace_compacted_history`. Coherence under this shape is
//! judged from a live-model transcript (harness in `codex-core`), not asserted
//! here.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use lhc::intake_stream::EventRecord;

/// How many trailing user_prompt events stay in the full band.
pub const DEFAULT_FULL_BAND_USER_TURNS: usize = 2;

/// One row of the eval transcript / dry-run dump.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BandShapeReport {
    pub source_event_count: usize,
    pub full_band_user_turns: usize,
    pub items: Vec<BandShapeItem>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BandShapeItem {
    pub band: String,
    pub role: String,
    pub text_preview: String,
    pub char_len: usize,
}

/// Build a minimal but realistic band-shaped `ResponseItem` history from
/// stored LHC events (Chunk 1 capture consumer).
///
/// Prefer `user_prompt` / `assistant_text` events; other kinds contribute to
/// band summaries as short lines.
pub fn band_shaped_history_from_events(
    events: &[EventRecord],
    full_band_user_turns: usize,
) -> (Vec<ResponseItem>, BandShapeReport) {
    let mut turns: Vec<TurnLine> = Vec::new();
    for ev in events {
        let kind = ev.event_kind().as_str();
        let text = event_text(ev);
        if text.trim().is_empty() {
            continue;
        }
        let role = match kind {
            "user_prompt" => TurnRole::User,
            "assistant_text" | "assistant_thinking" => TurnRole::Assistant,
            "tool_call" | "tool_result" => TurnRole::Tool,
            _ => TurnRole::Note,
        };
        turns.push(TurnLine {
            role,
            kind: kind.to_string(),
            text,
        });
    }

    let mut notes = Vec::new();
    if turns.is_empty() {
        notes.push("no mappable events; emitting synthetic minimal band fixture".into());
        let items = synthetic_minimal_band_history();
        let report = report_from_items(&items, 0, full_band_user_turns, notes);
        return (items, report);
    }

    // Split full band: last N user turns and everything after the Nth-from-end user.
    let user_idxs: Vec<usize> = turns
        .iter()
        .enumerate()
        .filter(|(_, t)| t.role == TurnRole::User)
        .map(|(i, _)| i)
        .collect();
    let full_start = if user_idxs.len() <= full_band_user_turns {
        notes.push(format!(
            "only {} user turns; entire transcript stays in full band",
            user_idxs.len()
        ));
        0
    } else {
        user_idxs[user_idxs.len() - full_band_user_turns]
    };

    let head = &turns[..full_start];
    let full = &turns[full_start..];

    let mut items: Vec<ResponseItem> = Vec::new();
    if !head.is_empty() {
        let n = head.len();
        let brief_end = (n / 3).max(1).min(n);
        let detailed_end = ((2 * n) / 3).max(brief_end).min(n);

        let brief = summarize_band("brief", &head[..brief_end]);
        let detailed = if detailed_end > brief_end {
            Some(summarize_band("detailed", &head[brief_end..detailed_end]))
        } else {
            None
        };
        let smooth = if n > detailed_end {
            Some(summarize_band("smooth", &head[detailed_end..]))
        } else {
            None
        };

        items.push(developer_note(&brief));
        if let Some(d) = detailed {
            items.push(developer_note(&d));
        }
        if let Some(s) = smooth {
            items.push(developer_note(&s));
        }
        notes.push(format!(
            "head_turns={n} → brief/detailed/smooth developer notes; full_turns={}",
            full.len()
        ));
    } else {
        notes.push("no head; only full band".into());
    }

    for line in full {
        match line.role {
            TurnRole::User => items.push(user_message(&line.text)),
            TurnRole::Assistant => items.push(assistant_message(&line.text)),
            TurnRole::Tool | TurnRole::Note => {
                // Keep tool/note as developer context so the model still sees them.
                items.push(developer_note(&format!(
                    "[lhc-band:full:{}] {}",
                    line.kind,
                    truncate(&line.text, 400)
                )));
            }
        }
    }

    let report = report_from_items(&items, events.len(), full_band_user_turns, notes);
    (items, report)
}

/// Tiny fixture used when no capture is available (still exercises multi-band shape).
pub fn synthetic_minimal_band_history() -> Vec<ResponseItem> {
    vec![
        developer_note(
            "[lhc-band:brief] Early work: explored repo layout and located compact ladder.",
        ),
        developer_note(
            "[lhc-band:detailed] Mid work: wire RawItemContributor capture; idempotency keys settled.",
        ),
        developer_note(
            "[lhc-band:smooth] Recent: certification round-trips green; pre-bridge census underway.",
        ),
        user_message("Continue from the census — what should the LHC compact arm do first?"),
        assistant_message(
            "Place the LHC arm above TokenBudget, write back via replace_compacted_history, and fail open to native arms.",
        ),
        user_message("Ack. Ready for band-shape model eval when auth is approved."),
    ]
}

// ── internals ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnRole {
    User,
    Assistant,
    Tool,
    Note,
}

struct TurnLine {
    role: TurnRole,
    kind: String,
    text: String,
}

fn event_text(ev: &EventRecord) -> String {
    if let Some(tp) = ev.text_payload() {
        return tp.text.clone();
    }
    if let Some(tc) = ev.tool_call_payload() {
        return format!(
            "tool_call {} {}",
            tc.tool_name,
            serde_json::to_string(&tc.arguments).unwrap_or_default()
        );
    }
    if let Some(tr) = ev.tool_result_payload() {
        return format!("tool_result {}", tr.content);
    }
    String::new()
}

fn summarize_band(band: &str, lines: &[TurnLine]) -> String {
    let mut parts = Vec::new();
    for line in lines.iter().take(8) {
        parts.push(format!(
            "- ({}) {}",
            line.kind,
            truncate(line.text.trim(), 120)
        ));
    }
    if lines.len() > 8 {
        parts.push(format!("- … {} more lines", lines.len() - 8));
    }
    format!("[lhc-band:{band}] {}", parts.join(" "))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let t: String = s.chars().take(max).collect();
    format!("{t}…")
}

fn developer_note(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".into(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn item_preview(item: &ResponseItem) -> (String, String, usize) {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let text = content
                .iter()
                .filter_map(|c| match c {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let band = text
                .strip_prefix("[lhc-band:")
                .and_then(|r| r.split(']').next())
                .unwrap_or("full")
                .to_string();
            (band, role.clone(), text.chars().count())
        }
        _ => ("other".into(), "other".into(), 0),
    }
}

fn report_from_items(
    items: &[ResponseItem],
    source_event_count: usize,
    full_band_user_turns: usize,
    notes: Vec<String>,
) -> BandShapeReport {
    let items = items
        .iter()
        .map(|item| {
            let (band, role, char_len) = item_preview(item);
            let text_preview = match item {
                ResponseItem::Message { content, .. } => content
                    .iter()
                    .filter_map(|c| match c {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(truncate(text, 160))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            BandShapeItem {
                band,
                role,
                text_preview,
                char_len,
            }
        })
        .collect();
    BandShapeReport {
        source_event_count,
        full_band_user_turns,
        items,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_history_has_all_bands_and_full_tail() {
        let items = synthetic_minimal_band_history();
        let report = report_from_items(&items, 0, DEFAULT_FULL_BAND_USER_TURNS, vec![]);
        let bands: Vec<_> = report.items.iter().map(|i| i.band.as_str()).collect();
        assert!(bands.contains(&"brief"));
        assert!(bands.contains(&"detailed"));
        assert!(bands.contains(&"smooth"));
        assert!(bands.iter().any(|b| *b == "full"));
        assert!(
            report.items.iter().any(|i| i.role == "user"),
            "full band must retain a user turn"
        );
    }
}
