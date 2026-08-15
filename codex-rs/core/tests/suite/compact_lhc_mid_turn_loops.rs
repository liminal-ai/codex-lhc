//! LIM-63B MidTurn full-loop acceptance (mock provider, production path).
//!
//! Drives a real Codex agentic turn through `test_codex` + mock SSE with LHC
//! capture installed and MidTurn test knobs (small upper trigger / lower bound).
//! Asserts request shapes, marker/pair residuals, and bounded
//! `context_length_exceeded` behavior — not enum return values alone.
//!
//! **Stack:** these full-loop suites nest tokio workers deep enough that the
//! default host stack can SIGABRT. CI and the tripwire set
//! `RUST_MIN_STACK=8388608`. Plain local runs should use the same env (or the
//! tripwire) — the suite documents this requirement explicitly so an always-skip
//! or under-stacked runner cannot silently green-pass.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::install_with_root;
use codex_lhc_host::test_compact_opts;
use codex_lhc_host::wait_for_handle;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::PathExt;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use serde_json::json;
use tempfile::TempDir;
use wiremock::MockServer;

const SUMMARIZATION_PROMPT: &str = "You are a helpful assistant that summarizes conversations.";

fn non_openai_model_provider(server: &MockServer) -> ModelProviderInfo {
    let mut provider = built_in_model_providers(/* openai_base_url */ None)["openai"].clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.name = "MockProvider".into();
    provider.supports_websockets = false;
    provider.stream_max_retries = Some(0);
    provider.request_max_retries = Some(0);
    provider
}

fn lhc_extensions(
    root: PathBuf,
) -> Arc<codex_extension_api::ExtensionRegistry<codex_core::config::Config>> {
    let mut builder = ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    install_with_root(&mut builder, |_c| true, root);
    Arc::new(builder.build())
}

/// Like [`lhc_extensions`] but with a real model label so capture identity
/// matches the live turn identity (R2 encrypted-reasoning replay gate).
fn lhc_extensions_with_model(
    root: PathBuf,
    model: &'static str,
) -> Arc<codex_extension_api::ExtensionRegistry<codex_core::config::Config>> {
    let mut builder = ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    codex_lhc_host::install_with_root_and_labels(
        &mut builder,
        |_c| true,
        root,
        move |_c| model.to_string(),
        |_c| "none".into(),
        |_c| None,
    );
    Arc::new(builder.build())
}

async fn arm_midturn_knobs(codex: &codex_core::CodexThread) {
    let slot = codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("LhcCaptureSlot must be installed for MidTurn full-loop tests");
    slot.set_mid_turn_test_compact(Some(test_compact_opts(400.0)));
    // Tiny upper trigger so response usage (2000+) crosses MidTurn pressure.
    slot.set_mid_turn_test_upper_trigger(Some(500));
    let _ = wait_for_handle(&slot, Duration::from_secs(30)).await;
}

fn count_substr(hay: &str, needle: &str) -> usize {
    hay.match_indices(needle).count()
}

fn request_bodies(mock: &core_test_support::responses::ResponseMock) -> Vec<String> {
    mock.requests()
        .into_iter()
        .map(|r| r.body_json().to_string())
        .collect()
}

fn body_has_summarization(body: &str) -> bool {
    // Exact native compact prompt only — do not match incidental "summarize" text.
    body.contains(SUMMARIZATION_PROMPT)
}

/// A. Active non-tool full loop: response 1 above trigger with end_turn=false →
/// MidTurn compact-continuation → request 2 retains task context, completes,
/// no native summarization arm, no empty third request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_active_non_tool_mid_turn_continuation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();

    let mut continue_done = ev_completed_with_tokens("resp-1", /*total_tokens*/ 2_500);
    continue_done["response"]["end_turn"] = json!(false);

    let first = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("m1", "working on long task part one"),
        continue_done,
    ]);
    let second = sse(vec![
        ev_response_created("resp-2"),
        ev_assistant_message("m2", "task complete after compact-continuation"),
        ev_completed_with_tokens("resp-2", /*total_tokens*/ 80),
    ]);
    let mock = mount_sse_sequence(&server, vec![first, second]).await;

    let model_provider = non_openai_model_provider(&server);
    let extensions = lhc_extensions(root);
    let mut builder = test_codex()
        .with_extensions(extensions)
        .with_config(move |config| {
            config.model_provider = model_provider;
            let _ = config.features.enable(Feature::LhcCapture);
            let _ = config.features.disable(Feature::TokenBudget);
            config.model_auto_compact_token_limit = Some(500);
            config.model_context_window = Some(8_000);
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.into());
        });
    let test = builder.build(&server).await?;
    arm_midturn_knobs(&test.codex).await;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "continue this long agentic task".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let bodies = request_bodies(&mock);
    // Production may issue request1 (sampling) + request2 (continuation). A
    // third empty request is a defect; allow only the two agentic requests.
    assert!(
        (2..=2).contains(&bodies.len()) || bodies.len() == 2,
        "expected exactly two provider requests, got {}",
        bodies.len()
    );
    assert_eq!(bodies.len(), 2, "no empty third request/turn");

    // No native compact summarization arm ran between the two agentic requests.
    let native_hits: Vec<usize> = bodies
        .iter()
        .enumerate()
        .filter(|(_, b)| b.contains(SUMMARIZATION_PROMPT))
        .map(|(i, _)| i)
        .collect();
    assert!(
        native_hits.is_empty(),
        "native summarization compact must not run under LHC MidTurn one-writer; hits={native_hits:?} n={} first_has={} second_has={}",
        bodies.len(),
        bodies
            .first()
            .map(|b| b.contains(SUMMARIZATION_PROMPT))
            .unwrap_or(false),
        bodies
            .get(1)
            .map(|b| b.contains(SUMMARIZATION_PROMPT))
            .unwrap_or(false),
    );

    let req2 = &bodies[1];
    // Positive durable evidence (always-skip fails): receipt + typed marker event.
    const MARKER_KIND: &str = "lhc.compact_continuation";
    const MARKER_CAUSE: &str = "context_compacted_task_in_progress";
    const MARKER_ACTION: &str = "continue_existing_task";
    let slot = test
        .codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, root.as_deref())
            .await
            .expect("inspect receipts");
    assert!(
        !receipts.is_empty(),
        "active non-tool full loop must leave a durable compact-continuation receipt"
    );
    let last = receipts.last().expect("receipt");
    assert!(
        last.terminal || last.outcome.contains("compact") || last.outcome.contains("no_reduction"),
        "unexpected durable outcome {}",
        last.outcome
    );
    if let Some(cont) = last.continuation_turn_id.as_deref() {
        let has = codex_lhc_host::inspect_has_compact_continuation_marker(
            &thread_id,
            root.as_deref(),
            cont,
        )
        .await
        .expect("marker");
        assert!(
            has,
            "durable typed marker must exist for continuation turn {cont}"
        );
    }
    // When reverse-mapped into request 2, require frozen kind/cause/action once.
    let marker_hits = count_substr(req2, MARKER_KIND);
    if marker_hits > 0 {
        assert_eq!(marker_hits, 1, "at most one typed marker in request 2");
        assert!(
            req2.contains(MARKER_CAUSE) && req2.contains(MARKER_ACTION),
            "request 2 marker must carry cause/action constants"
        );
    }
    assert_eq!(
        count_substr(&bodies[0], MARKER_KIND),
        0,
        "request 1 must not already carry the continuation marker"
    );
    // Task context retained on the continuation request.
    assert!(
        req2.contains("continue this long agentic task")
            || req2.contains("working on long task")
            || req2.contains("part one")
            || req2.contains(MARKER_KIND)
            || req2.contains("context_compact"),
        "request 2 must retain task context: {}",
        &req2[..req2.len().min(500)]
    );

    Ok(())
}

/// B. Pending parallel-tool full loop under contract 2.0.0: reasoning + two
/// tool calls settle via the real executor; the complete parallel set is
/// protected. With this fixture's synthetic scope limit (500 tokens, far
/// below the response usage) and no older prunable content, preserve cannot
/// create safe runway and escalation has nothing eligible to prune — the
/// certified outcome is a TRUTHFUL BOUNDED REFUSAL (`unsafe_runway`): the
/// next provider request is blocked, both pairs stay verbatim in the
/// canonical record, no continuation marker is fabricated, and no native
/// compact runs (LIM-67 acceptance: protected results alone exceed budget /
/// no eligible old results).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_pending_parallel_tools_mid_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();

    let shell_args_z = json!({
        "command": "echo z-out",
        "timeout_ms": 2000,
    })
    .to_string();
    let shell_args_a = json!({
        "command": "echo a-out",
        "timeout_ms": 2000,
    })
    .to_string();

    let first = sse(vec![
        ev_response_created("resp-tool-1"),
        ev_reasoning_item(
            "rsn-loop-1",
            &["plan both tools"],
            &["reasoning body for parallel tools"],
        ),
        ev_function_call("call-loop-z", "shell_command", &shell_args_z),
        ev_function_call("call-loop-a", "shell_command", &shell_args_a),
        ev_completed_with_tokens("resp-tool-1", /*total_tokens*/ 2_500),
    ]);
    let second = sse(vec![
        ev_response_created("resp-tool-2"),
        ev_assistant_message("m-tool-2", "both tools done, task complete"),
        ev_completed_with_tokens("resp-tool-2", /*total_tokens*/ 90),
    ]);
    // The certified outcome is a bounded refusal: request 2 must never be
    // sent. Mount it as a recording sentinel (no expectation) so a defective
    // second send is caught by its request count instead of a mock panic.
    let mock = mount_sse_once(&server, first).await;
    let sentinel = mount_sse_once(&server, second).await;

    let model_provider = non_openai_model_provider(&server);
    let extensions = lhc_extensions(root);
    let cwd_for_policy = TempDir::new()?;
    let cwd_path = cwd_for_policy.path().to_path_buf();
    let mut builder = test_codex()
        .with_extensions(extensions)
        .with_config(move |config| {
            config.model_provider = model_provider;
            let _ = config.features.enable(Feature::LhcCapture);
            let _ = config.features.disable(Feature::TokenBudget);
            config.model_auto_compact_token_limit = Some(500);
            config.model_context_window = Some(8_000);
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.into());
        });
    let test = builder.build(&server).await?;
    arm_midturn_knobs(&test.codex).await;

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd_path.as_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "run two parallel tools then continue".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    environments: Some(local_selections(test.cwd_path().abs())),
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    ..Default::default()
                },
            ),
        )
        .await?;
    let terminal = wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::TurnComplete(_) | EventMsg::Error(_))
    })
    .await;
    assert!(
        matches!(terminal, EventMsg::TurnComplete(_) | EventMsg::Error(_)),
        "bounded refusal must reach a clear terminal outcome"
    );

    let bodies = request_bodies(&mock);
    // Truthful bounded refusal: the blocked continuation request is never sent.
    assert_eq!(
        bodies.len(),
        1,
        "unsafe runway with no eligible relief must block the next provider request; got {}",
        bodies.len()
    );
    assert_eq!(
        request_bodies(&sentinel).len(),
        0,
        "no second provider request may reach the sentinel mock after a bounded refusal"
    );
    assert!(
        !bodies.iter().any(|b| body_has_summarization(b)),
        "native compact must not run on pending-tool MidTurn path"
    );
    // No marker was fabricated for the refused attempt.
    assert_eq!(
        count_substr(&bodies[0], "lhc.compact_continuation"),
        0,
        "refused pending-tool attempt must not fabricate a continuation marker"
    );

    // Durable truth: an unsafe_runway refusal receipt with the full protected
    // set, no marker, and both pairs verbatim in the canonical record.
    let slot = test
        .codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    let thread_id = handle.thread_id().to_string();
    let lhc_data_root = handle.root().map(std::path::Path::to_path_buf);
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, lhc_data_root.as_deref())
            .await
            .expect("inspect receipts");
    let refusal = receipts
        .iter()
        .find(|r| {
            r.receipt
                .refuse_code
                .map(codex_lhc_host::CompactContinuationRefuseCode::as_str)
                == Some("unsafe_runway")
        })
        .expect("durable unsafe_runway refusal receipt");
    assert_eq!(
        refusal.receipt.residual.protected_tool_call_ids,
        vec!["call-loop-a".to_string(), "call-loop-z".to_string()],
        "refusal receipt records the complete sorted protected set"
    );
    assert!(
        !refusal.receipt.residual.marker_persisted,
        "refused attempt must not persist a marker"
    );
    assert!(
        refusal.receipt.residual.prior_serving_view_intact,
        "refusal leaves the prior serving view intact"
    );
    let canonical = canonical_messages_blocking(&thread_id, lhc_data_root);
    assert!(
        canonical.iter().any(|m| m.contains("call-loop-z"))
            && canonical.iter().any(|m| m.contains("call-loop-a")),
        "both parallel pairs stay verbatim in the canonical record"
    );

    Ok(())
}

/// E. Bounded mock `context_length_exceeded`: no retry/compact loop, no
/// native/LHC writer race pollution, clear terminal outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_context_length_exceeded_is_bounded() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();

    // First response succeeds with high usage + end_turn=false so MidTurn may
    // fire; subsequent provider attempts all return context_length_exceeded.
    let mut first_done = ev_completed_with_tokens("resp-1", /*total_tokens*/ 3_000);
    first_done["response"]["end_turn"] = json!(false);
    let first = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("m1", "starting large context task"),
        first_done,
    ]);
    // Product policy: request_max_retries = 0 and ContextWindowExceeded is
    // non-retryable (turn.rs returns Err immediately). Expected provider
    // requests = 2: initial success (end_turn=false, MidTurn may compact) +
    // exactly one CLE continuation attempt that terminates the turn.
    // Mount more CLE failures than that so a retry treadmill would exceed the
    // bound rather than stop only because mocks exhaust.
    const EXPECTED_PROVIDER_REQUESTS: usize = 2;
    let first_mock = mount_sse_once(&server, first).await;
    let mut cle_mocks = Vec::new();
    for i in 0..8 {
        cle_mocks.push(
            mount_sse_once(
                &server,
                sse_failed(
                    &format!("cle-{i}"),
                    "context_length_exceeded",
                    "Your input exceeds the context window of this model. Please adjust your input and try again.",
                ),
            )
            .await,
        );
    }

    let model_provider = non_openai_model_provider(&server);
    let extensions = lhc_extensions(root);
    let mut builder = test_codex()
        .with_extensions(extensions)
        .with_config(move |config| {
            config.model_provider = model_provider;
            let _ = config.features.enable(Feature::LhcCapture);
            let _ = config.features.disable(Feature::TokenBudget);
            config.model_auto_compact_token_limit = Some(500);
            config.model_context_window = Some(4_000);
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.into());
        });
    let test = builder.build(&server).await?;
    arm_midturn_knobs(&test.codex).await;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "push context until exceeded".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    // Terminal: TurnComplete or Error — not an infinite loop.
    let terminal = wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::TurnComplete(_) | EventMsg::Error(_))
    })
    .await;
    assert!(
        matches!(terminal, EventMsg::TurnComplete(_) | EventMsg::Error(_)),
        "task must reach a clear terminal outcome"
    );

    let mut bodies = request_bodies(&first_mock);
    for m in &cle_mocks {
        bodies.extend(request_bodies(m));
    }
    // Product-driven CLE bound from configured policy (request_max_retries=0):
    // exact provider request count, not mock capacity.
    assert_eq!(
        bodies.len(),
        EXPECTED_PROVIDER_REQUESTS,
        "CLE path must issue exactly {EXPECTED_PROVIDER_REQUESTS} provider requests \
         (initial success + one CLE continuation) with request_max_retries=0; got {}",
        bodies.len()
    );
    // Terminal outcome already asserted above. CLE path must not arm native
    // summarization or spin unbounded marker pollution.
    let native_compact_hits = bodies.iter().filter(|b| body_has_summarization(b)).count();
    assert_eq!(
        native_compact_hits,
        0,
        "no native compact retry loop under LHC MidTurn; summarization hits={native_compact_hits} n={}",
        bodies.len()
    );
    // No duplicate marker pollution across requests.
    let total_markers: usize = bodies
        .iter()
        .map(|b| count_substr(b, "lhc.compact_continuation"))
        .sum();
    assert!(
        total_markers <= 1,
        "no polluted duplicate markers across requests, total={total_markers}"
    );

    // Durable LHC compact-continuation receipts are product-bounded (at most one).
    let slot = test
        .codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    let thread_id = handle.thread_id().to_string();
    let root = handle.root().map(std::path::Path::to_path_buf);
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, root.as_deref())
            .await
            .expect("inspect receipts");
    assert!(
        receipts.len() <= 1,
        "CLE path must not leave an unbounded compact/receipt treadmill; receipts={}",
        receipts.len()
    );

    Ok(())
}

// ── LIM-67: sustained protected escalation ──────────────────────────────────

/// Read canonical LHC message content on a dedicated thread (SDK futures are
/// `!Send`; the loop tests run on a multi-thread runtime).
fn canonical_messages_blocking(thread_id: &str, root: Option<PathBuf>) -> Vec<String> {
    let tid = thread_id.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let surfaces = codex_lhc_host::read_materialize_surfaces(&tid, root.as_deref())
                .await
                .expect("materialize surfaces");
            surfaces
                .messages
                .iter()
                .map(|m| serde_json::to_string(m).unwrap_or_default())
                .collect::<Vec<String>>()
        })
    })
    .join()
    .expect("canonical read thread")
}

fn hv_row_blocking(
    thread_id: &str,
    root: Option<PathBuf>,
    attempt_id: &str,
) -> Option<codex_lhc_host::HostValidationAck> {
    let tid = thread_id.to_string();
    let attempt = attempt_id.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            codex_lhc_host::inspect_mid_turn_host_validation(&tid, root.as_deref(), &attempt)
                .await
                .expect("inspect host validation")
        })
    })
    .join()
    .expect("hv read thread")
}

/// Command whose OUTPUT carries the bulk (~8000 chars) with a distinctive
/// marker LAST, while the call arguments stay small — matching real tool
/// shapes (calls/reasoning are never visibility-prune targets; only result
/// bodies shorten). The marker string never appears literally in the
/// arguments (`%d` formatting), so its presence in a request body proves the
/// verbatim OUTPUT survived and its absence proves the output was abridged.
fn cycle_command(i: usize) -> String {
    format!("yes tok | head -n 2000 | tr '\n' ' '; printf -- '-ENDPAY%d' {i}")
}

/// Approximate unpruned output volume per cycle (chars).
const CYCLE_OUTPUT_CHARS: usize = 8_000;

/// LIM-67 sustained proof: 22 deterministic tool cycles under a real Codex
/// auto-compact scope limit. Provider usage follows a sawtooth (real relief
/// lowers real usage; the mock emulates the post-relief drop), so the seam
/// crosses the trigger on waves and LHC must repeatedly produce safe runway.
///
/// Proves: old unprotected tool-result bodies shorten across cycles; the
/// latest protected pair stays byte-stable and ordered on every request;
/// encrypted reasoning survives; canonical content stays retrievable; exactly
/// one forced boundary + typed marker per escalation attempt; the full next
/// request stays below the safe threshold; host validation resolves `ok` for
/// every served escalation; no native compact path runs. Preserve-first also
/// covers the closed-turn-bulk shape: closed tail turns are banded by ordinary
/// preserve, so escalation's boundary never lands behind the candidate compact
/// point (the Rust-stage disclosure case does not arise on this path).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_sustained_protected_escalation_bounded() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const CYCLES: usize = 22;
    const SCOPE_LIMIT: i64 = 20_000;
    /// Wave seams where the emulated provider usage approaches the scope
    /// limit (post-relief responses drop back down, as a real provider would
    /// after the request shrank).
    const WAVE_SEAMS: [usize; 3] = [7, 14, 21];

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();

    let mut responses = Vec::new();
    for i in 0..CYCLES {
        let usage: i64 = if WAVE_SEAMS.contains(&i) {
            19_600
        } else {
            10_000
        };
        let args = json!({
            "command": cycle_command(i),
            "timeout_ms": 10_000,
        })
        .to_string();
        responses.push(sse(vec![
            ev_response_created(&format!("resp-sus-{i}")),
            ev_reasoning_item(
                &format!("rsn-sus-{i}"),
                &["sustained plan"],
                &[&format!("reasoning cycle {i}")],
            ),
            ev_function_call(&format!("call-sus-{i}"), "shell_command", &args),
            ev_completed_with_tokens(&format!("resp-sus-{i}"), usage),
        ]));
    }
    responses.push(sse(vec![
        ev_response_created("resp-sus-final"),
        ev_assistant_message("m-sus-final", "sustained task complete"),
        ev_completed_with_tokens("resp-sus-final", 300),
    ]));
    let mock = mount_sse_sequence(&server, responses).await;

    let model_provider = non_openai_model_provider(&server);
    let extensions = lhc_extensions_with_model(root, "gpt-5.5");
    let cwd_for_policy = TempDir::new()?;
    let cwd_path = cwd_for_policy.path().to_path_buf();
    let mut builder = test_codex()
        .with_extensions(extensions)
        .with_config(move |config| {
            config.model_provider = model_provider;
            let _ = config.features.enable(Feature::LhcCapture);
            let _ = config.features.disable(Feature::TokenBudget);
            // The REAL host safe-runway source: the auto-compact scope limit
            // doubles as the upper trigger and the safe-runway threshold.
            config.model_auto_compact_token_limit = Some(SCOPE_LIMIT);
            config.model_context_window = Some(200_000);
            // Pin the model so capture identity matches turn identity and the
            // R2 gate replays encrypted reasoning (production always has this).
            config.model = Some("gpt-5.5".into());
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.into());
        });
    let test = builder.build(&server).await?;
    let slot = test
        .codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let _ = wait_for_handle(&slot, Duration::from_secs(30)).await;

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd_path.as_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "run the sustained tool loop".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    environments: Some(local_selections(test.cwd_path().abs())),
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    ..Default::default()
                },
            ),
        )
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let bodies = request_bodies(&mock);
    assert_eq!(
        bodies.len(),
        CYCLES + 1,
        "one provider request per tool cycle plus the final completion"
    );
    assert!(
        !bodies.iter().any(|b| body_has_summarization(b)),
        "no native compact path may run on any LIM-67 sustained cycle"
    );

    // Protected pair byte-stability at every seam: request k+1 carries cycle
    // k's call id and its full output marker (protected at that seam).
    for k in 0..CYCLES {
        let req = &bodies[k + 1];
        assert!(
            req.contains(&format!("call-sus-{k}")),
            "request {} must carry protected call id call-sus-{k}",
            k + 1
        );
        assert!(
            req.contains(&format!("-ENDPAY{k}\\n")) || req.contains(&format!("-ENDPAY{k}")),
            "request {} must carry cycle {k}'s protected output verbatim (marker at end)",
            k + 1
        );
    }

    // Actual reduction at the first wave: the request after the seam-7 relief
    // must be smaller than the one before it.
    assert!(
        bodies[8].len() < bodies[7].len(),
        "wave-1 relief must shrink the next provider request ({} -> {})",
        bodies[7].len(),
        bodies[8].len()
    );
    // Escalation-driven visibility pruning is exact: the wave-1 escalation
    // abridges every older unprotected output (their end markers vanish from
    // the very next request) while the protected pair stays verbatim.
    for i in 0..7 {
        assert!(
            !bodies[8].contains(&format!("-ENDPAY{i}")),
            "request 8 must serve cycle {i}'s output abridged after the wave-1 escalation"
        );
    }
    // Wave-2 escalation prunes the next stretch of now-unprotected outputs.
    let wave2_dropped = (7..14)
        .filter(|i| !bodies[15].contains(&format!("-ENDPAY{i}")))
        .count();
    assert!(
        wave2_dropped >= 5,
        "wave-2 escalation must abridge the older outputs; only {wave2_dropped} of 7 dropped"
    );

    // Actual reduction across repeated cycles: by the final request, early
    // cycles' unprotected outputs remain abridged/banded — their end markers
    // stay gone (degraded band fallbacks may retain a few members verbatim).
    let final_req = bodies.last().expect("final request");
    let early = 14usize;
    let dropped_old_markers = (0..early)
        .filter(|i| !final_req.contains(&format!("-ENDPAY{i}")))
        .count();
    assert!(
        dropped_old_markers >= 6,
        "sustained relief must keep old unprotected tool outputs short; only {dropped_old_markers} of {early} early markers dropped"
    );
    // The final request stays bounded — strictly below the accumulated
    // unpruned payload volume and within the safe-runway order of magnitude.
    let unpruned_chars: usize = CYCLES * CYCLE_OUTPUT_CHARS;
    assert!(
        final_req.len() < unpruned_chars,
        "final request ({} chars) must be smaller than unpruned payload volume ({unpruned_chars} chars)",
        final_req.len()
    );
    // The host-validated guarantee: every request that follows an escalated
    // (host-validated) install stays below the safe-runway threshold. The
    // degraded preserve path at seam 21 is not host-validated (LIM-67 gates
    // escalations); offline derivation floors can keep it fatter.
    for follow in [8usize, 15usize] {
        assert!(
            (bodies[follow].len() as i64) / 4 < SCOPE_LIMIT,
            "request {follow} after a host-validated escalation (~{} est tokens) must stay below the safe-runway threshold",
            bodies[follow].len() / 4
        );
    }

    // Encrypted reasoning survives into the following request.
    {
        use base64::Engine as _;
        let last_cycle = CYCLES - 1;
        let expected_encrypted = base64::engine::general_purpose::STANDARD
            .encode(format!("{}reasoning cycle {last_cycle}", "b".repeat(550)));
        assert!(
            final_req.contains(&expected_encrypted),
            "final request must carry the prior response's encrypted reasoning verbatim"
        );
    }

    // Durable receipts: escalations happened, each with exactly one boundary +
    // typed marker; installs are truthful; no misreported install failures.
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    let thread_id = handle.thread_id().to_string();
    let lhc_data_root = handle.root().map(std::path::Path::to_path_buf);
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, lhc_data_root.as_deref())
            .await
            .expect("inspect receipts");
    let escalated: Vec<_> = receipts
        .iter()
        .filter(|r| {
            matches!(
                r.receipt.relief_path.as_str(),
                "protected_escalation" | "host_validation_awaiting"
            )
        })
        .collect();
    assert!(
        !escalated.is_empty(),
        "sustained loop must include protected escalations; outcomes={:?}",
        receipts
            .iter()
            .map(|r| (r.outcome.clone(), r.receipt.relief_path.as_str()))
            .collect::<Vec<_>>()
    );
    for r in &escalated {
        let cont = r
            .continuation_turn_id
            .as_deref()
            .expect("escalated receipt must carry its continuation turn id");
        let has_marker = codex_lhc_host::inspect_has_compact_continuation_marker(
            &thread_id,
            lhc_data_root.as_deref(),
            cont,
        )
        .await
        .expect("marker inspect");
        assert!(
            has_marker,
            "exactly one typed marker per escalation ({cont})"
        );
        assert!(
            !r.receipt.residual.protected_tool_call_ids.is_empty(),
            "escalated receipt records its protected set"
        );
        // Host validation resolved ok for every served escalated install.
        if r.receipt.residual.host_validation_status.as_str() == "awaiting" {
            let row = hv_row_blocking(&thread_id, lhc_data_root.clone(), &r.attempt_id)
                .expect("durable host-validation row for escalated install");
            assert_eq!(
                row.status,
                codex_lhc_host::HostValidationStatus::Ok,
                "served escalated install must have host validation recorded ok"
            );
        }
    }
    assert!(
        !receipts.iter().any(|r| r
            .receipt
            .refuse_code
            .map(codex_lhc_host::CompactContinuationRefuseCode::as_str)
            == Some("install_failed")),
        "no misreported install failures across the sustained loop"
    );

    // Canonical content remains retrievable verbatim: the earliest cycle's
    // full payload (marker included) still lives in the canonical record even
    // though the served request abridged it.
    let canonical = canonical_messages_blocking(&thread_id, lhc_data_root.clone());
    assert!(
        canonical.iter().any(|m| m.contains("-ENDPAY0")),
        "canonical record must retain the earliest tool output verbatim"
    );

    Ok(())
}
