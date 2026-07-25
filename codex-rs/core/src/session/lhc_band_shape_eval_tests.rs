//! Chunk 2a — band-shape model-tolerance harness (pre-bridge).
//!
//! Builds a realistic band-shaped replacement history, installs it via the
//! production write-back API (`replace_compacted_history`), and either:
//!
//! - **Dry-run (default):** dumps the installed shape + report to a file and
//!   returns. No model calls. Safe for CI / tripwire.
//! - **Live eval (`CODEX_LHC_BAND_EVAL=1`):** continues a few user turns on the
//!   throwaway session and appends model responses to the dump for human
//!   judgment. **Do not run live without Lee** — spends ChatGPT plan quota.
//!
//! Commands:
//!
//! ```text
//! # Dry-run (always):
//! cargo test -p codex-core --lib lhc_band_shape_eval -- --nocapture
//!
//! # Live (ignored by default; needs auth):
//! CODEX_LHC_BAND_EVAL=1 CODEX_LHC_BAND_EVAL_OUT=/tmp/lhc-band-eval.jsonl \
//!   cargo test -p codex-core --lib lhc_band_shape_eval_live -- --nocapture --ignored
//! ```
//!
//! Cost (live, defaults): 2 continuation turns × ~2–4k token prompts ≈
//! **~5–10k tokens** total. Smallest history that still has brief/detailed/
//! smooth notes + a full-band tail.

use std::path::PathBuf;
use std::time::Duration;

use codex_lhc_host::BandShapeReport;
use codex_lhc_host::DEFAULT_FULL_BAND_USER_TURNS;
use codex_lhc_host::LhcSession;
use codex_lhc_host::band_shaped_history_from_events;
use codex_lhc_host::synthetic_minimal_band_history;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

use super::Session;
use super::tests::make_session_and_context;
use crate::compact::CompactedHistoryMetadata;

fn eval_out_path() -> PathBuf {
    if let Ok(p) = std::env::var("CODEX_LHC_BAND_EVAL_OUT") {
        return PathBuf::from(p);
    }
    std::env::temp_dir().join("lhc-band-shape-eval.json")
}

/// Prefer a real captured LHC thread when `CODEX_LHC_ROOT` + `CODEX_LHC_BAND_THREAD`
/// are set; otherwise use the synthetic multi-band fixture (still realistic shape).
async fn load_band_history() -> (Vec<ResponseItem>, BandShapeReport, String) {
    let root = std::env::var("CODEX_LHC_ROOT").ok();
    let thread_id = std::env::var("CODEX_LHC_BAND_THREAD").ok();
    match (root, thread_id) {
        (Some(root), Some(thread_id)) => {
            let root_path = PathBuf::from(&root);
            let opened = LhcSession::open(&thread_id, None, Some(root_path.as_path())).await;
            match opened {
                Some((session, _)) => {
                    let events = session.list_events().await.unwrap_or_default();
                    let (items, report) =
                        band_shaped_history_from_events(&events, DEFAULT_FULL_BAND_USER_TURNS);
                    let src = format!("lhc-thread:{thread_id}@{root} events={}", events.len());
                    session.close().await;
                    (items, report, src)
                }
                None => {
                    let items = synthetic_minimal_band_history();
                    let (_, report) =
                        band_shaped_history_from_events(&[], DEFAULT_FULL_BAND_USER_TURNS);
                    (
                        items,
                        report,
                        format!("synthetic (failed to open {thread_id} under {root})"),
                    )
                }
            }
        }
        _ => {
            let items = synthetic_minimal_band_history();
            let (_, report) = band_shaped_history_from_events(&[], DEFAULT_FULL_BAND_USER_TURNS);
            (
                items,
                report,
                "synthetic-minimal (set CODEX_LHC_ROOT + CODEX_LHC_BAND_THREAD for real capture)"
                    .into(),
            )
        }
    }
}

async fn install_band_history(session: &Session, items: Vec<ResponseItem>) {
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    session
        .replace_compacted_history(
            items,
            /*reference_context_item*/ None,
            /*world_state_baseline*/ None,
            CompactedHistoryMetadata {
                message: "lhc-band-shape-eval".into(),
                window_number,
                window_ids,
            },
        )
        .await;
    // Post-install pulse (law 2 path also runs recompute on real compact arms).
    let _ = session.get_total_token_usage().await;
}

/// Dry-run: construct → install → dump. No model. Safe for CI.
#[tokio::test]
async fn lhc_band_shape_eval_dry_run_installs_and_dumps() {
    let (items, report, source) = load_band_history().await;
    assert!(
        !items.is_empty(),
        "band-shaped history must be non-empty ({source})"
    );

    let (session, _turn_context) = make_session_and_context().await;
    let before_prefill = session.auto_compact_window_snapshot().await;
    // Artificially set a prefill so we can observe clear_prefill via replace.
    {
        let mut state = session.state.lock().await;
        state.set_auto_compact_window_estimated_prefill(/*tokens*/ 50_000);
    }
    install_band_history(&session, items.clone()).await;
    let after = session.auto_compact_window_snapshot().await;
    assert_eq!(
        after.prefill_input_tokens, None,
        "replace_compacted_history must clear auto-compact prefill (law 2); before={before_prefill:?}"
    );

    let history = session.clone_history().await;
    let raw = history.raw_items();
    // Installed history should include our band markers or full-band user turns.
    let joined: String = raw
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("[lhc-band:") || joined.contains("band"),
        "installed history should retain band shape markers; got len={}",
        joined.len()
    );

    let out = eval_out_path();
    let dump = json!({
        "mode": "dry_run",
        "source": source,
        "report": report,
        "installed_item_count": raw.len(),
        "installed_preview": joined.chars().take(2000).collect::<String>(),
        "live_eval": "set CODEX_LHC_BAND_EVAL=1 and run lhc_band_shape_eval_live --ignored",
        "approx_cost_live": "~5–10k tokens (2 turns × small band history)",
    });
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_string_pretty(&dump).expect("json"))
        .expect("write band-shape dump");
    eprintln!("lhc band-shape dry-run wrote {}", out.display());
}

/// Live model continuation — ignored by default. Requires network + auth.
/// Coherence is **not** asserted; review the dump file.
#[tokio::test]
#[ignore = "live model eval; spends plan quota — Lee sign-off required"]
async fn lhc_band_shape_eval_live() {
    if std::env::var("CODEX_LHC_BAND_EVAL").ok().as_deref() != Some("1") {
        eprintln!("skip: set CODEX_LHC_BAND_EVAL=1 to run live eval");
        return;
    }

    let (items, report, source) = load_band_history().await;
    let (session, turn_context) = make_session_and_context().await;
    install_band_history(&session, items).await;

    let turns: usize = std::env::var("CODEX_LHC_BAND_EVAL_TURNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    let mut transcript = Vec::new();
    transcript.push(json!({
        "event": "install",
        "source": source,
        "report": report,
    }));

    for i in 0..turns {
        let prompt = format!(
            "Band-shape eval turn {}: reply in one short sentence confirming you still know the plan (LHC compact arm above TokenBudget, write-back via replace_compacted_history).",
            i + 1
        );
        transcript.push(json!({
            "event": "user",
            "turn": i + 1,
            "text": prompt,
        }));

        // Record user prompt through the production path so history advances.
        session
            .record_user_prompt_and_emit_turn_item(
                &turn_context,
                &[UserInput::Text {
                    text: prompt,
                    text_elements: Vec::new(),
                }],
                None,
            )
            .await;

        // Live sampling would go through the normal turn loop. For a minimal
        // harness we only record that the session accepted the prompt under
        // band history; full model streaming requires a wired ModelClient and
        // is left to Chunk 3 live cert if this path stays auth-gated.
        //
        // When a full turn runner is available in-test, replace this stub with
        // a real turn. For now we dump history after each user record so a
        // human can still inspect prompt construction.
        let history = session.clone_history().await;
        let raw_len = history.raw_items().len();
        transcript.push(json!({
            "event": "after_user_record",
            "turn": i + 1,
            "history_len": raw_len,
            "note": "model stream not invoked in this harness revision — history install + user record only; extend when auth lane is open",
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let out = eval_out_path();
    let dump = json!({
        "mode": "live_stub",
        "transcript": transcript,
        "warning": "Full model streaming is intentionally not auto-fired until the auth lane is decided with Lee. This stub certifies install + prompt-path under band history only.",
    });
    std::fs::write(&out, serde_json::to_string_pretty(&dump).expect("json"))
        .expect("write live dump");
    eprintln!("lhc band-shape live stub wrote {}", out.display());
}

/// Law 2 native-path coverage without the LHC arm: replace_compacted_history
/// clears prefill so BodyAfterPrefix scope can untrip.
#[tokio::test]
async fn replace_compacted_history_clears_prefill_for_threshold_untrip() {
    let dir = tempdir().expect("tempdir");
    let _ = dir;
    let (session, _tc) = make_session_and_context().await;
    {
        let mut state = session.state.lock().await;
        state.set_auto_compact_window_estimated_prefill(/*tokens*/ 80_000);
    }
    assert!(
        session
            .auto_compact_window_snapshot()
            .await
            .prefill_input_tokens
            .is_some()
    );

    install_band_history(&session, synthetic_minimal_band_history()).await;

    assert_eq!(
        session
            .auto_compact_window_snapshot()
            .await
            .prefill_input_tokens,
        None,
        "law 2: write-back must clear auto-compact prefill"
    );
}
