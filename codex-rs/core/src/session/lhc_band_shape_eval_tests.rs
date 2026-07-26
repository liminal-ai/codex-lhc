//! Band-shape model-tolerance harness (Chunk 2a/2b → round 6 / K2).
//!
//! **Dry-run:** install a real `lhc.compact`-produced body (via the production
//! arm with test-only deterministic derivation for offline CI), dump shape.
//!
//! **Live eval (`CODEX_LHC_BAND_EVAL=1`):** same real body path, then continue
//! with non-leading + control questions on ChatGPT auth / `gpt-5.6-luna` /
//! lowest effort. Coherence is **not** asserted in code — read the dump.
//!
//! ```text
//! cargo test -p codex-core --lib lhc_band_shape_eval -- --nocapture
//!
//! CODEX_LHC_BAND_EVAL=1 CODEX_LHC_BAND_EVAL_OUT=/tmp/lhc-band-eval.json \
//!   cargo test -p codex-core --lib lhc_band_shape_eval_live -- --nocapture --ignored
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadStartInput;
use codex_features::Feature;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::install_with_root;
use codex_lhc_host::wait_for_handle;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

use super::Session;
use super::tests::make_session_and_context;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::compact::CompactedHistoryMetadata;
use crate::compact::InitialContextInjection;
use crate::compact_lhc::LhcCompactAttempt;
use crate::compact_lhc::try_run_lhc_compact_arm;
use crate::lhc_inference_bridge::LHC_DERIVATION_MODEL;
use crate::lhc_inference_bridge::resolve_lhc_derivation_effort;
use crate::responses_metadata::CodexResponsesMetadata;
use tokio_util::sync::CancellationToken;

/// Distinctive facts planted in seed history. Eval questions must **not** name these.
const PROJECT_CODENAME: &str = "Quokka-Nimbus-417";
const WORK_STAGE: &str = "scaffolding the silver-thread handshake";
const CRITICAL_FILE: &str = "quokka_bridge.rs";
const RECENT_NEXT_STEP: &str = "land the silver-thread handshake integration test";
/// Control: never appears in seed or body. Confident answers here are confabulation.
const CONTROL_ABSENT: &str = "purple elephant on Mars";

fn eval_out_path() -> PathBuf {
    if let Ok(p) = std::env::var("CODEX_LHC_BAND_EVAL_OUT") {
        return PathBuf::from(p);
    }
    std::env::temp_dir().join("lhc-band-shape-eval.json")
}

fn text_input(text: &str) -> UserInput {
    UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }
}

async fn install_lhc_and_enable(session: &mut Session, root: PathBuf) {
    session
        .set_feature_for_test(Feature::LhcCapture, true)
        .expect("enable LhcCapture");
    let mut builder = ExtensionRegistryBuilder::<crate::config::Config>::new();
    install_with_root(&mut builder, |_c| true, root);
    let registry = Arc::new(builder.build());
    session.services.extensions = Arc::clone(&registry);
    let config = session.get_config().await;
    let environments = [];
    let session_source = SessionSource::Exec;
    for contributor in registry.thread_lifecycle_contributors() {
        contributor
            .on_thread_start(ThreadStartInput {
                config: config.as_ref(),
                session_source: &session_source,
                persistent_thread_state_available: false,
                environments: &environments,
                mcp_resource_client: None,
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
            })
            .await;
    }
}

/// Seed a bandable conversation with distinctive facts (not generic pad-only).
async fn seed_distinctive_bandable(
    session: &Session,
    tc: &crate::session::turn_context::TurnContext,
    bulk_turns: usize,
) {
    // Pad enough for LHC banding lower_bound, but keep post-compact body
    // from blowing eval cost past ~10k tokens (pad survives in smooth excerpts).
    let pad = "p".repeat(900);
    // Early facts (likely compressed into brief/smooth bands under LHC).
    let early = [
        format!(
            "Kickoff: we are building project {PROJECT_CODENAME}. Current stage is {WORK_STAGE}."
        ),
        format!(
            "Architecture note for {PROJECT_CODENAME}: the critical implementation file is {CRITICAL_FILE}."
        ),
        format!(
            "Constraint for {PROJECT_CODENAME}: do not touch the legacy amber pipeline; only {CRITICAL_FILE}."
        ),
    ];
    for (i, user) in early.iter().enumerate() {
        session
            .record_user_prompt_and_emit_turn_item(tc, &[text_input(user)], None)
            .await;
        session
            .record_conversation_items_with_provenance(
                tc,
                &[ResponseItem::Message {
                    id: None,
                    role: "assistant".into(),
                    content: vec![ContentItem::OutputText {
                        text: format!("ack early-{i}: noted {PROJECT_CODENAME} / {CRITICAL_FILE}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                codex_extension_api::RawItemProvenance::ModelOutput,
            )
            .await;
    }
    // Bulk to exceed LHC banding lower bound.
    for i in 0..bulk_turns {
        let user = format!("bulk progress turn {i} on {PROJECT_CODENAME} {pad}");
        session
            .record_user_prompt_and_emit_turn_item(tc, &[text_input(&user)], None)
            .await;
        session
            .record_conversation_items_with_provenance(
                tc,
                &[ResponseItem::Message {
                    id: None,
                    role: "assistant".into(),
                    content: vec![ContentItem::OutputText {
                        text: format!("bulk reply {i} continuing {WORK_STAGE} {pad}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                codex_extension_api::RawItemProvenance::ModelOutput,
            )
            .await;
    }
    // Recent full-band tail: distinctive next step (should remain verbatim).
    let recent = [
        format!("Status check on {PROJECT_CODENAME}: still in {WORK_STAGE}."),
        format!("Agreed next step: {RECENT_NEXT_STEP}."),
        format!("Confirming focus file remains {CRITICAL_FILE} for the handshake work."),
    ];
    for (i, user) in recent.iter().enumerate() {
        session
            .record_user_prompt_and_emit_turn_item(tc, &[text_input(user)], None)
            .await;
        session
            .record_conversation_items_with_provenance(
                tc,
                &[ResponseItem::Message {
                    id: None,
                    role: "assistant".into(),
                    content: vec![ContentItem::OutputText {
                        text: format!("ack recent-{i}: will proceed as discussed."),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                codex_extension_api::RawItemProvenance::ModelOutput,
            )
            .await;
    }
}

fn body_preview(items: &[ResponseItem], max_chars: usize) -> String {
    let joined: String = items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, role, .. } => {
                let text = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                Some(format!("[{role}] {text}"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    joined.chars().take(max_chars).collect()
}

fn band_composition(items: &[ResponseItem]) -> serde_json::Value {
    let mut context_smooth = 0usize;
    let mut context_other = 0usize;
    let mut user = 0usize;
    let mut assistant = 0usize;
    let mut other = 0usize;
    let mut has_project = false;
    let mut has_file = false;
    let mut has_next = false;
    let mut has_control = false;
    for item in items {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                if text.contains("[context ·") {
                    if text.contains("smooth") {
                        context_smooth += 1;
                    } else {
                        context_other += 1;
                    }
                } else if role == "user" {
                    user += 1;
                } else if role == "assistant" {
                    assistant += 1;
                } else {
                    other += 1;
                }
                if text.contains(PROJECT_CODENAME) {
                    has_project = true;
                }
                if text.contains(CRITICAL_FILE) {
                    has_file = true;
                }
                if text.contains(RECENT_NEXT_STEP) || text.contains("silver-thread handshake") {
                    has_next = true;
                }
                if text.contains(CONTROL_ABSENT) {
                    has_control = true;
                }
            }
            _ => other += 1,
        }
    }
    json!({
        "items": items.len(),
        "context_smooth_like": context_smooth,
        "context_other": context_other,
        "user_messages": user,
        "assistant_messages": assistant,
        "other": other,
        "body_contains_project_codename": has_project,
        "body_contains_critical_file": has_file,
        "body_contains_recent_next_step": has_next,
        "body_contains_control_absent_fact": has_control,
    })
}

/// Produce a real LHC compact body via the production arm (deterministic
/// derivation for offline cost). Returns (body, source_event_count, archive root meta).
async fn produce_real_lhc_body() -> (Vec<ResponseItem>, usize, String, serde_json::Value) {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_and_enable(&mut session, root.clone()).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    // Background derivation runs on these; without seeding, the arm waits out
    // its settle bound and fails open (see compact_lhc::SETTLE_WAIT).
    slot.set_derivation_callbacks(
        codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic callbacks"),
    );
    seed_distinctive_bandable(&session, &tc, 80).await;
    handle.flush().await;
    assert!(
        handle.drain_settled(Duration::from_secs(180)).await,
        "background derivation must settle before the eval compacts"
    );

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(|p| p.to_path_buf());
    let source_events = archive_source_event_count(&thread_id, root_path.as_deref()).await;

    // Test-only deterministic override so CI/dry-run can produce a real
    // `lhc.compact` body without live derivation spend. Structure is production
    // produce path (not synthetic_minimal_band_history).
    *session.services.lhc_test_inference.lock().expect("lock") = Some(
        codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic for body produce"),
    );

    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ true,
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    let LhcCompactAttempt::Installed { body, marker } = attempt else {
        panic!("band-scale produce must Install real LHC body: {attempt:?}");
    };
    assert!(!body.is_empty());
    let joined = body_preview(&body, 8000);
    assert!(
        joined.contains("[context ·") || marker.body_item_count > 0,
        "real LHC body should carry context-band markers; preview={}",
        joined.chars().take(400).collect::<String>()
    );
    // Must not be the hand-written synthetic fixture text.
    assert!(
        !joined.contains("[lhc-band:brief] Early work: explored repo layout"),
        "must not use synthetic_minimal_band_history fixture"
    );

    let composition = band_composition(&body);
    let source = format!(
        "lhc.compact production arm (deterministic derivation for body produce); \
         thread={thread_id} source_events={source_events} body_items={} \
         marker_total_tokens={} compact_point={}",
        body.len(),
        marker.total_tokens,
        marker.compact_point
    );
    let _ = dir;
    (body, source_events, source, composition)
}

async fn archive_source_event_count(thread_id: &str, root: Option<&std::path::Path>) -> usize {
    let Ok(callbacks) = codex_lhc_host::lhc_inference_callbacks(false) else {
        return 0;
    };
    let Some((session, _)) =
        codex_lhc_host::LhcSession::open_with_inference(thread_id, None, root, callbacks).await
    else {
        return 0;
    };
    let Ok(events) = session.list_events().await else {
        session.close().await;
        return 0;
    };
    session.close().await;
    events
        .iter()
        .filter(|e| {
            matches!(
                e.event_kind().as_str(),
                "user_prompt" | "assistant_text" | "assistant_thinking"
            )
        })
        .count()
}

/// Dry-run: real LHC body → install → dump. No model spend.
#[tokio::test]
async fn lhc_band_shape_eval_dry_run_installs_and_dumps() {
    let (body, source_events, source, composition) = produce_real_lhc_body().await;
    assert!(source_events > 0, "seed must leave archive events");

    let (session, _tc) = make_session_and_context().await;
    {
        let mut state = session.state.lock().await;
        state.set_auto_compact_window_estimated_prefill(/*tokens*/ 50_000);
    }
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    session
        .replace_compacted_history(
            body.clone(),
            /*reference_context_item*/ None,
            /*world_state_baseline*/ None,
            CompactedHistoryMetadata {
                message: "lhc-band-shape-eval".into(),
                window_number,
                window_ids,
            },
        )
        .await;
    assert_eq!(
        session
            .auto_compact_window_snapshot()
            .await
            .prefill_input_tokens,
        None
    );

    let out = eval_out_path();
    let dump = json!({
        "mode": "dry_run",
        "source": source,
        "source_event_count": source_events,
        "band_composition": composition,
        "installed_item_count": body.len(),
        "installed_preview": body_preview(&body, 2500),
        "planted_facts": {
            "project": PROJECT_CODENAME,
            "stage": WORK_STAGE,
            "file": CRITICAL_FILE,
            "recent_next_step": RECENT_NEXT_STEP,
            "control_absent": CONTROL_ABSENT,
        },
        "live_eval": "CODEX_LHC_BAND_EVAL=1 cargo test -p codex-core --lib lhc_band_shape_eval_live -- --nocapture --ignored",
        "ruled_lane": {
            "model": LHC_DERIVATION_MODEL,
            "auth": "ChatGPT",
            "effort": "None if accepted else min (luna → low)",
        },
    });
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_string_pretty(&dump).expect("json")).expect("write dump");
    eprintln!(
        "lhc band-shape dry-run wrote {} source_events={source_events} body_items={}",
        out.display(),
        body.len()
    );
}

/// Live continuation on a **real** LHC-produced body. Non-leading + control Qs.
#[tokio::test]
#[ignore = "live model eval; spends plan quota — authorized under ruling 8c3ca18"]
async fn lhc_band_shape_eval_live() {
    if std::env::var("CODEX_LHC_BAND_EVAL").ok().as_deref() != Some("1") {
        eprintln!("skip: set CODEX_LHC_BAND_EVAL=1 to run live eval");
        return;
    }
    if std::env::var("CODEX_SANDBOX_NETWORK_DISABLED")
        .ok()
        .as_deref()
        == Some("1")
    {
        panic!("live eval needs network; CODEX_SANDBOX_NETWORK_DISABLED=1");
    }

    let (body, source_events, source, composition) = produce_real_lhc_body().await;
    assert!(
        source_events >= 100,
        "need real scale archive, got source_events={source_events}"
    );
    assert!(
        !matches!(
            composition.get("body_contains_control_absent_fact"),
            Some(serde_json::Value::Bool(true))
        ),
        "control fact must not appear in body"
    );

    let codex_home = dirs::home_dir()
        .map(|h| h.join(".codex"))
        .filter(|p| p.is_dir())
        .or_else(|| std::env::var("CODEX_HOME").ok().map(PathBuf::from))
        .expect("CODEX_HOME or ~/.codex required");

    let config = crate::config::ConfigBuilder::default()
        .codex_home(codex_home.clone())
        .build()
        .await
        .expect("load config");
    let auth_manager = codex_login::AuthManager::shared_from_config(
        &config, /*enable_codex_api_key_env*/ true,
    )
    .await;
    let auth = auth_manager.auth().await.expect("ChatGPT auth required");
    eprintln!(
        "lhc band-shape live K2: auth={:?} model={LHC_DERIVATION_MODEL} source_events={source_events} body_items={}",
        auth.auth_mode(),
        body.len()
    );

    let model_info = {
        let mm = crate::thread_manager::build_models_manager(&config, Arc::clone(&auth_manager));
        mm.get_model_info(LHC_DERIVATION_MODEL, &config.to_models_manager_config())
            .await
    };
    assert!(
        !model_info.used_fallback_model_metadata,
        "luna must resolve"
    );
    let effort = resolve_lhc_derivation_effort(&model_info);

    use crate::client::ModelClient;
    use codex_login::auth::AgentIdentityAuthPolicy;
    use codex_protocol::ThreadId;
    use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
    use codex_protocol::protocol::SessionSource;
    use codex_rollout_trace::InferenceTraceContext;

    let thread_id = ThreadId::new();
    let client = ModelClient::new(
        Some(auth_manager),
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id,
        config.model_provider.clone(),
        SessionSource::Exec,
        "lhc-band-shape-eval".to_string(),
        config.model_verbosity,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        config.http_client_factory(),
    );

    let (session, _tc) = make_session_and_context().await;
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    session
        .replace_compacted_history(
            body.clone(),
            /*reference_context_item*/ None,
            /*world_state_baseline*/ None,
            CompactedHistoryMetadata {
                message: "lhc-band-shape-eval-live".into(),
                window_number,
                window_ids,
            },
        )
        .await;

    // Non-leading questions: do **not** name expected answers.
    // Control: something never in the body.
    let questions = [
        (
            "recall",
            "What were we working on before this, and what stage is that work at? Answer briefly from conversation context only.",
        ),
        (
            "control",
            "What was decided about the purple elephant on Mars? Answer briefly from conversation context only; if it was never discussed, say so.",
        ),
    ];

    let mut conversation = body;
    let mut transcript = vec![json!({
        "event": "install",
        "source": source,
        "source_event_count": source_events,
        "band_composition": composition,
        "model": model_info.slug,
        "effort": effort.to_string(),
        "auth_mode": format!("{:?}", auth.auth_mode()),
        "planted_facts_for_human_judgement_only": {
            "project": PROJECT_CODENAME,
            "stage": WORK_STAGE,
            "file": CRITICAL_FILE,
            "recent_next_step": RECENT_NEXT_STEP,
            "control_absent": CONTROL_ABSENT,
            "note": "Do not treat model answers as pass/fail in CI; judge recovery vs confabulation by hand.",
        },
    })];

    let mut total_input: i64 = 0;
    let mut total_output: i64 = 0;
    let mut total_tokens: i64 = 0;
    let telemetry = session.services.session_telemetry.clone();

    for (label, prompt_text) in questions {
        conversation.push(ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: prompt_text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        });
        transcript.push(json!({
            "event": "user",
            "label": label,
            "text": prompt_text,
        }));

        let prompt = Prompt {
            input: conversation.clone(),
            ..Default::default()
        };
        let meta = CodexResponsesMetadata::new(
            "lhc-band-eval".into(),
            thread_id.to_string(),
            thread_id.to_string(),
            format!("{thread_id}:band-eval:{label}"),
        );
        let mut client_session = client.new_session();
        let stream = client_session
            .stream(
                &prompt,
                &model_info,
                &telemetry,
                Some(effort.clone()),
                ReasoningSummaryConfig::None,
                /*service_tier*/ None,
                &meta,
                &InferenceTraceContext::disabled(),
            )
            .await
            .expect("live stream");

        let mut out = String::new();
        let mut turn_usage = None;
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ResponseEvent::OutputTextDelta(delta)) => out.push_str(&delta),
                Ok(ResponseEvent::OutputItemDone(item)) => {
                    if let ResponseItem::Message { content, .. } = &item {
                        for part in content {
                            if let ContentItem::OutputText { text } = part {
                                if out.is_empty() {
                                    out = text.clone();
                                }
                            }
                        }
                    }
                    conversation.push(item);
                }
                Ok(ResponseEvent::Completed { token_usage, .. }) => {
                    turn_usage = token_usage;
                }
                Ok(_) => {}
                Err(err) => panic!("stream error ({label}): {err}"),
            }
        }
        assert!(!out.is_empty(), "empty response for {label}");
        if let Some(u) = &turn_usage {
            total_input += u.input_tokens;
            total_output += u.output_tokens;
            total_tokens += u.total_tokens;
        }
        transcript.push(json!({
            "event": "assistant",
            "label": label,
            "text": out,
            "token_usage": turn_usage.as_ref().map(|u| json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "total_tokens": u.total_tokens,
            })),
        }));
        eprintln!(
            "lhc band-shape live [{label}]: chars={} usage={:?}",
            out.len(),
            turn_usage
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let out = eval_out_path();
    let dump = json!({
        "mode": "live",
        "model": model_info.slug,
        "effort": effort.to_string(),
        "source_event_count": source_events,
        "band_composition": composition,
        "turns_completed": questions.len(),
        "token_usage_total": {
            "input_tokens": total_input,
            "output_tokens": total_output,
            "total_tokens": total_tokens,
        },
        "transcript": transcript,
        "judgement": {
            "recall": "Does the model recover project/stage/file/next-step from band-shaped LHC body without being told the answers?",
            "control": "Does it correctly say the purple elephant was never discussed (or confabulate)?",
            "rule": "If recall fails or control confabulates confidently, report negative — do not mitigate in this round.",
        },
    });
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_string_pretty(&dump).expect("json"))
        .expect("write live dump");
    eprintln!(
        "lhc band-shape LIVE K2 wrote {} — turns={} total_tokens={total_tokens} \
         (input={total_input} output={total_output}) source_events={source_events}",
        out.display(),
        questions.len()
    );
}

/// Law 2: replace_compacted_history clears prefill.
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
    // Minimal install — law2 only cares about prefill clear, not band source.
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    session
        .replace_compacted_history(
            vec![ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![ContentItem::InputText {
                    text: "placeholder".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            /*reference_context_item*/ None,
            /*world_state_baseline*/ None,
            CompactedHistoryMetadata {
                message: "law2".into(),
                window_number,
                window_ids,
            },
        )
        .await;
    assert_eq!(
        session
            .auto_compact_window_snapshot()
            .await
            .prefill_input_tokens,
        None,
        "law 2: write-back must clear auto-compact prefill"
    );
}
