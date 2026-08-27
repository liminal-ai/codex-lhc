//! MidTurn full-loop acceptance (mock provider, production path).
//!
//! Drives a real Codex agentic turn through `test_codex` + mock SSE with LHC
//! capture installed and MidTurn test knobs (small upper trigger / lower bound).
//! Asserts request shapes, marker/pair residuals, and bounded
//! `context_length_exceeded` behavior — not enum return values alone.
//!
//! Turn parts (Story 5): a clean thread's MidTurn relief is the certified
//! parts compact inside the same Codex turn. These loops assert what that
//! rules out on a clean thread — no continuation turn, no typed
//! compact-continuation marker in any request, no forced-boundary receipt —
//! alongside the request-shape invariants LIM-63B already pinned. The legacy
//! runtime remains covered by the unit suite through its typed-only route.
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
/// MidTurn parts compact in the same turn → request 2 retains task context,
/// completes, no native summarization arm, no continuation marker, no empty
/// third request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_active_non_tool_mid_turn_same_turn() -> Result<()> {
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
        ev_assistant_message("m2", "task complete after mid-turn compact"),
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
    // Turn parts (Story 5): a clean thread never takes the forced-boundary
    // path. No typed compact-continuation marker in either request and no
    // receipt naming a continuation turn.
    const MARKER_KIND: &str = "lhc.compact_continuation";
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
        receipts.iter().all(|r| r.continuation_turn_id.is_none()),
        "clean thread must not open a continuation turn: {receipts:?}"
    );
    for (i, body) in bodies.iter().enumerate() {
        assert_eq!(
            count_substr(body, MARKER_KIND),
            0,
            "request {i} must not carry the forced-boundary marker"
        );
    }
    // Task context retained on the next request of the same turn.
    assert!(
        req2.contains("continue this long agentic task")
            || req2.contains("working on long task")
            || req2.contains("part one"),
        "request 2 must retain task context: {}",
        &req2[..req2.len().min(500)]
    );

    Ok(())
}

/// B. Pending parallel-tool full loop: reasoning + two tool calls settle via
/// the real executor; the complete parallel set is protected. Body-size is
/// no longer a terminal unsafe_runway refuse — continuation must send a
/// second provider request with both pairs and encrypted reasoning intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_pending_parallel_tools_mid_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();

    let shell_args_z = json!({
        "cmd": "echo z-out",
        "yield_time_ms": 2000,
    })
    .to_string();
    let shell_args_a = json!({
        "cmd": "echo a-out",
        "yield_time_ms": 2000,
    })
    .to_string();

    let first = sse(vec![
        ev_response_created("resp-tool-1"),
        ev_reasoning_item(
            "rsn-loop-1",
            &["plan both tools"],
            &["reasoning body for parallel tools"],
        ),
        ev_function_call("call-loop-z", "exec_command", &shell_args_z),
        ev_function_call("call-loop-a", "exec_command", &shell_args_a),
        ev_completed_with_tokens("resp-tool-1", /*total_tokens*/ 2_500),
    ]);
    let second = sse(vec![
        ev_response_created("resp-tool-2"),
        ev_assistant_message("m-tool-2", "both tools done, task complete"),
        ev_completed_with_tokens("resp-tool-2", /*total_tokens*/ 90),
    ]);
    let mock = mount_sse_sequence(&server, vec![first, second]).await;

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
            config.model_auto_compact_token_limit = Some(500);
            config.model_context_window = Some(8_000);
            config.model = Some("gpt-5.5".into());
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
        matches!(
            ev,
            EventMsg::TurnComplete(_) | EventMsg::Error(_) | EventMsg::TurnAborted(_)
        )
    })
    .await;
    assert!(
        matches!(terminal, EventMsg::TurnComplete(_)),
        "continuation must reach TurnComplete (not abort/error), got {terminal:?}"
    );

    let bodies = request_bodies(&mock);
    assert_eq!(
        bodies.len(),
        2,
        "one initial request plus one after continuation; got {}",
        bodies.len()
    );
    assert!(
        !bodies.iter().any(|b| body_has_summarization(b)),
        "native compact must not run on pending-tool MidTurn path"
    );

    let req2 = &bodies[1];
    assert!(
        req2.contains("call-loop-z") && req2.contains("call-loop-a"),
        "request 2 must carry both protected parallel call ids"
    );
    assert!(
        req2.contains("z-out"),
        "request 2 must carry call-loop-z output"
    );
    assert!(
        req2.contains("a-out"),
        "request 2 must carry call-loop-a output"
    );
    {
        use base64::Engine as _;
        let expected_encrypted = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}reasoning body for parallel tools",
            "b".repeat(550)
        ));
        assert!(
            req2.contains(&expected_encrypted),
            "request 2 must carry required encrypted reasoning"
        );
    }

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
    assert!(
        receipts.iter().all(|r| r.continuation_turn_id.is_none()),
        "pending parallel tools settle inside the same turn; no continuation turn: {receipts:?}"
    );
    assert_eq!(
        count_substr(req2, "lhc.compact_continuation"),
        0,
        "request 2 must not carry the forced-boundary marker"
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

    // Terminal: TurnComplete, Error, or TurnAborted (Slice B blocked next
    // provider) — not an infinite loop.
    let terminal = wait_for_event(&test.codex, |ev| {
        matches!(
            ev,
            EventMsg::TurnComplete(_) | EventMsg::Error(_) | EventMsg::TurnAborted(_)
        )
    })
    .await;
    assert!(
        matches!(
            terminal,
            EventMsg::TurnComplete(_) | EventMsg::Error(_) | EventMsg::TurnAborted(_)
        ),
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
    assert!(
        receipts.iter().all(|r| r.continuation_turn_id.is_none()),
        "CLE relief on a clean thread never forces a boundary: {receipts:?}"
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

/// Command whose OUTPUT carries the bulk (~8000 chars) with a distinctive
/// marker LAST, while the call arguments stay small — matching real tool
/// shapes (calls/reasoning are never visibility-prune targets; only result
/// bodies shorten). The marker string never appears literally in the
/// arguments (`%d` formatting), so its presence in a request body proves the
/// verbatim OUTPUT survived and its absence proves the output was abridged.
fn cycle_command(i: usize) -> String {
    format!("yes tok | head -n 8000 | tr '\n' ' '; printf -- '-ENDPAY%d' {i}")
}

/// Approximate unpruned output volume per cycle (chars).
const CYCLE_OUTPUT_CHARS: usize = 32_000;

/// LIM-67 sustained proof under turn parts: 22 deterministic tool cycles whose
/// outputs cross the SDK's production lower bound part-way through. Provider
/// usage follows a sawtooth so the seam crosses the auto-compact trigger on
/// waves. The parts arm splits the active turn at step edges inside the one
/// Codex turn: every request k+1 still carries cycle k's protected pair
/// verbatim, the final request is bounded and abridges early outputs, encrypted
/// reasoning survives, the installed view serves parts and a later request is
/// served across the seam — with no continuation turn and no forced-boundary
/// marker anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_sustained_pressure_parts_bounded() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const CYCLES: usize = 22;
    const LATER_CYCLES: usize = 18;
    // Slice C counts serialized JSON with o200k, not chars/4. 80k still
    // triggers at the wave seams while leaving room for the accurate estimate.
    const SCOPE_LIMIT: i64 = 80_000;
    /// Wave seams where the emulated provider usage approaches the scope
    /// limit (post-relief responses drop back down, as a real provider would
    /// after the request shrank).
    const WAVE_SEAMS: [usize; 3] = [7, 14, 21];
    const LATER_WAVE_SEAMS: [usize; 3] = [6, 12, 17];
    const NEXT_PROMPT: &str = "continue with a small follow-up, then keep working";

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();

    let mut responses = Vec::new();
    for i in 0..CYCLES {
        let usage: i64 = if WAVE_SEAMS.contains(&i) {
            78_400
        } else {
            40_000
        };
        let args = json!({
            "cmd": cycle_command(i),
            "yield_time_ms": 10_000,
        })
        .to_string();
        responses.push(sse(vec![
            ev_response_created(&format!("resp-sus-{i}")),
            ev_reasoning_item(
                &format!("rsn-sus-{i}"),
                &["sustained plan"],
                &[&format!("reasoning cycle {i}")],
            ),
            ev_function_call(&format!("call-sus-{i}"), "exec_command", &args),
            ev_completed_with_tokens(&format!("resp-sus-{i}"), usage),
        ]));
    }
    responses.push(sse(vec![
        ev_response_created("resp-sus-final"),
        ev_assistant_message("m-sus-final", "sustained task complete"),
        ev_completed_with_tokens("resp-sus-final", 300),
    ]));
    let tiny_args = json!({
        "cmd": "printf NEXT-TINY-COMPLETE",
        "yield_time_ms": 10_000,
    })
    .to_string();
    responses.push(sse(vec![
        ev_response_created("resp-next-tiny"),
        ev_function_call("call-next-tiny", "exec_command", &tiny_args),
        ev_completed_with_tokens("resp-next-tiny", 78_400),
    ]));
    for i in 0..LATER_CYCLES {
        let usage: i64 = if LATER_WAVE_SEAMS.contains(&i) {
            78_400
        } else {
            40_000
        };
        let args = json!({
            "cmd": cycle_command(100 + i),
            "yield_time_ms": 10_000,
        })
        .to_string();
        responses.push(sse(vec![
            ev_response_created(&format!("resp-next-{i}")),
            ev_reasoning_item(
                &format!("rsn-next-{i}"),
                &["follow-up plan"],
                &[&format!("follow-up reasoning cycle {i}")],
            ),
            ev_function_call(&format!("call-next-{i}"), "exec_command", &args),
            ev_completed_with_tokens(&format!("resp-next-{i}"), usage),
        ]));
    }
    responses.push(sse(vec![
        ev_response_created("resp-next-final"),
        ev_assistant_message("m-next-final", "follow-up task complete"),
        ev_completed_with_tokens("resp-next-final", 300),
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
            // Auto-compact trigger only — not a body-size refuse.
            config.model_auto_compact_token_limit = Some(SCOPE_LIMIT);
            config.model_context_window = Some(400_000);
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
    // Turn parts: this loop runs the production parts arm under the SDK's
    // production lower bound (120k tokens; core's test knobs are not compiled
    // into integration builds), so the fixture's tool outputs are sized to
    // cross it part-way through the loop.
    let _ = wait_for_handle(&slot, Duration::from_secs(30)).await;

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd_path.as_path());
    let started = test
        .codex
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
    let codex_protocol::turn_input::TurnInputSubmission::Started {
        turn_id: first_host_turn_id,
    } = started
    else {
        panic!("first submission must start a turn, got {started:?}");
    };
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

    // Durable mechanism evidence (turn parts): the sustained relief split the
    // active turn into parts inside the one Codex turn — the installed view
    // serves parts, no continuation turn was opened, and no request carried
    // the forced-boundary marker.
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    handle.flush().await;
    let thread_id = handle.thread_id().to_string();
    let lhc_data_root = handle.root().map(std::path::Path::to_path_buf);
    let first_turn_id = handle
        .durable_turn_id(&first_host_turn_id)
        .expect("first host turn must be bound to its durable turn");
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, lhc_data_root.as_deref())
            .await
            .expect("inspect receipts");
    assert!(
        receipts.iter().all(|r| r.continuation_turn_id.is_none()),
        "sustained relief must never open a continuation turn: {receipts:?}"
    );
    assert!(
        !bodies
            .iter()
            .any(|b| b.contains("lhc.compact_continuation")),
        "no request may carry the forced-boundary marker"
    );
    let view = codex_lhc_host::inspect_installed_view(&thread_id, lhc_data_root.as_deref())
        .await
        .expect("describe")
        .expect("sustained relief installs a serving view");
    assert!(
        codex_lhc_host::view_serves_parts(&view),
        "the active turn is served as parts by the installed view"
    );
    let split_compact_point = view.compact_point;
    let split_parts: Vec<_> = view
        .arrangement
        .iter()
        .filter(|entry| entry.part.is_some())
        .cloned()
        .collect();
    assert!(
        !split_parts.is_empty()
            && split_parts
                .iter()
                .all(|entry| entry.subject_id == first_turn_id),
        "the first turn must be the sole unsettled turn: {view:?}"
    );
    assert!(
        bodies.iter().skip(1).any(|b| b.contains("[seam · ")),
        "a later request is served across the parts seam"
    );

    // Semantic pruning evidence: by the final request, some early-cycle
    // unprotected tool outputs have been abridged — their end markers are
    // gone. The selector may trade raw tail for bounded smooth/band context,
    // so immediate per-wave shrinkage is not guaranteed, but the final state
    // must show reduction.
    let final_req = bodies.last().expect("final request");
    let early = 14usize;
    let dropped_old_markers = (0..early)
        .filter(|i| !final_req.contains(&format!("-ENDPAY{i}")))
        .count();
    assert!(
        dropped_old_markers >= 3,
        "sustained relief must abridge some early tool outputs; only {dropped_old_markers} of {early} early markers dropped"
    );
    // The final request stays bounded — strictly below the accumulated
    // unpruned payload volume.
    let unpruned_chars: usize = CYCLES * CYCLE_OUTPUT_CHARS;
    assert!(
        final_req.len() < unpruned_chars,
        "final request ({} chars) must be smaller than unpruned payload volume ({unpruned_chars} chars)",
        final_req.len()
    );

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

    // Canonical content remains retrievable verbatim: the earliest cycle's
    // full payload (marker included) still lives in the canonical record even
    // though the served request abridged it.
    let canonical = canonical_messages_blocking(&thread_id, lhc_data_root.clone());
    assert!(
        canonical.iter().any(|m| m.contains("-ENDPAY0")),
        "canonical record must retain the earliest tool output verbatim"
    );

    // Close → next user turn (burn-in stage 2): start a genuine second Codex
    // turn, let one tiny tool step settle, and inspect the provider request
    // produced after the next compact. The prior closed turn must remain the
    // sole unsettled turn until newer Full-tail messages fill the budget.
    let next_started = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: NEXT_PROMPT.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let codex_protocol::turn_input::TurnInputSubmission::Started {
        turn_id: next_host_turn_id,
    } = next_started
    else {
        panic!("follow-up submission must start a new turn, got {next_started:?}");
    };
    wait_for_event(
        &test.codex,
        |ev| matches!(ev, EventMsg::ExecCommandBegin(event) if event.call_id == "call-next-0"),
    )
    .await;
    handle.flush().await;
    let next_turn_id = handle
        .durable_turn_id(&next_host_turn_id)
        .expect("follow-up host turn must be bound to its durable turn");
    assert_ne!(first_turn_id, next_turn_id, "a genuine new turn must open");

    let post_close_bodies = request_bodies(&mock);
    let post_close_request = &post_close_bodies[CYCLES + 2];
    let complete_old_suffix = format!("{}-ENDPAY21", "tok ".repeat(8000));
    assert_eq!(
        count_substr(post_close_request, &complete_old_suffix),
        1,
        "the first turn's complete nonempty late suffix must survive exactly once"
    );
    let suffix_at = post_close_request
        .find(&complete_old_suffix)
        .expect("complete old suffix in post-close request");
    let next_prompt_at = post_close_request
        .find(NEXT_PROMPT)
        .expect("new prompt in post-close request");
    assert!(
        suffix_at < next_prompt_at,
        "the old turn's Full suffix must precede the new prompt"
    );
    assert_eq!(
        count_substr(post_close_request, NEXT_PROMPT),
        1,
        "the new prompt must have exactly one served copy"
    );
    let post_close_view =
        codex_lhc_host::inspect_installed_view(&thread_id, lhc_data_root.as_deref())
            .await
            .expect("describe post-close view")
            .expect("post-close compact installs a view");
    assert_eq!(
        post_close_view.compact_point, split_compact_point,
        "the closed transition turn keeps its installed compact point"
    );
    assert_eq!(
        post_close_view
            .arrangement
            .iter()
            .filter(|entry| entry.part.is_some())
            .cloned()
            .collect::<Vec<_>>(),
        split_parts,
        "the closed transition turn keeps its installed parts"
    );
    assert!(
        post_close_view.gaps.is_empty()
            && post_close_view
                .arrangement
                .iter()
                .all(|entry| !entry.degraded),
        "post-close view must have no gap or degraded coverage: {post_close_view:?}"
    );
    assert!(
        post_close_view
            .arrangement
            .iter()
            .filter(|entry| entry.subject_id == first_turn_id)
            .all(|entry| entry.part.is_some()),
        "the old split turn must not also appear as a whole entry"
    );
    let post_close_bands: Vec<String> = post_close_view
        .bands
        .iter()
        .map(|band| {
            serde_json::to_value(&band.band)
                .expect("band json")
                .as_str()
                .unwrap()
                .into()
        })
        .collect();
    let gradient = ["brief", "detailed", "smooth"];
    assert!(
        post_close_bands.windows(2).all(|pair| {
            gradient
                .iter()
                .position(|band| *band == pair[0].as_str())
                .unwrap()
                < gradient
                    .iter()
                    .position(|band| *band == pair[1].as_str())
                    .unwrap()
        }),
        "bands must remain in brief → detailed → smooth order: {post_close_bands:?}"
    );

    // Once later Full-tail pressure reaches the settlement threshold, the
    // walk settles the old turn whole before it splits the active turn. The
    // resulting installed view proves the ordering atomically: old whole,
    // new parts, exactly one unsettled turn.
    wait_for_event(
        &test.codex,
        |ev| matches!(ev, EventMsg::ExecCommandBegin(event) if event.call_id == "call-next-13"),
    )
    .await;
    handle.flush().await;
    let settled_view = codex_lhc_host::inspect_installed_view(&thread_id, lhc_data_root.as_deref())
        .await
        .expect("describe settled view")
        .expect("later compact installs a settled view");
    let settled_old: Vec<_> = settled_view
        .arrangement
        .iter()
        .filter(|entry| entry.subject_id == first_turn_id)
        .collect();
    assert_eq!(
        settled_old.len(),
        1,
        "the old turn settles atomically whole"
    );
    assert_eq!(
        serde_json::to_value(&settled_old[0].band)?,
        json!("smooth"),
        "the old turn's settled construction belongs in Smooth"
    );
    assert!(
        !settled_old[0].degraded && settled_view.gaps.is_empty(),
        "settlement must preserve complete clean coverage"
    );
    let mut settled_unsettled: Vec<String> = settled_view
        .arrangement
        .iter()
        .filter(|entry| entry.part.is_some())
        .map(|entry| entry.subject_id.clone())
        .collect();
    settled_unsettled.sort();
    settled_unsettled.dedup();
    assert_eq!(
        settled_unsettled,
        vec![next_turn_id.clone()],
        "the old turn settles before the follow-up turn becomes the sole unsettled turn"
    );

    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    handle.flush().await;
    let final_view = codex_lhc_host::inspect_installed_view(&thread_id, lhc_data_root.as_deref())
        .await
        .expect("describe final view")
        .expect("later pressure installs a follow-up parts view");
    let mut final_unsettled: Vec<String> = final_view
        .arrangement
        .iter()
        .filter(|entry| entry.part.is_some())
        .map(|entry| entry.subject_id.clone())
        .collect();
    final_unsettled.sort();
    final_unsettled.dedup();
    assert_eq!(
        final_unsettled,
        vec![next_turn_id.clone()],
        "the follow-up turn must become the sole unsettled turn"
    );
    let final_old: Vec<_> = final_view
        .arrangement
        .iter()
        .filter(|entry| entry.subject_id == first_turn_id)
        .collect();
    assert_eq!(
        final_old.len(),
        1,
        "the settled old turn must have one whole construction"
    );
    assert!(
        final_old[0].part.is_none() && !final_old[0].degraded,
        "the old turn remains whole and nondegraded after later progress"
    );
    assert!(
        final_view.gaps.is_empty() && final_view.arrangement.iter().all(|entry| !entry.degraded),
        "later progress must retain gap-free, nondegraded coverage: {final_view:?}"
    );
    assert!(
        final_view.compact_point > settled_view.compact_point,
        "later pressure must advance the follow-up split point"
    );

    let all_bodies = request_bodies(&mock);
    assert_eq!(
        all_bodies.len(),
        CYCLES + LATER_CYCLES + 3,
        "both real turns must make the expected bounded provider progress"
    );
    assert!(
        all_bodies
            .last()
            .is_some_and(|body| body.contains("-ENDPAY117") && body.contains(NEXT_PROMPT)),
        "the final request must prove later follow-up progress"
    );
    assert!(
        !all_bodies.iter().any(|body| body_has_summarization(body)),
        "no native compact path may run across either turn"
    );
    assert!(
        !all_bodies
            .iter()
            .any(|body| body.contains("lhc.compact_continuation")),
        "no forced-boundary marker may appear across either turn"
    );
    let final_receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, lhc_data_root.as_deref())
            .await
            .expect("inspect final receipts");
    assert!(
        final_receipts
            .iter()
            .all(|receipt| receipt.continuation_turn_id.is_none()),
        "neither turn may open a continuation turn: {final_receipts:?}"
    );

    Ok(())
}

/// Read the durable turn/event record of `thread_id` as JSON (SDK futures are
/// `!Send`, so the read runs on its own thread). Returns `(turns, events)` in
/// record order.
fn durable_record_blocking(
    thread_id: &str,
    root: Option<PathBuf>,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let tid = thread_id.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let callbacks =
                codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic callbacks");
            let (session, _) =
                codex_lhc_host::LhcSession::open(&tid, None, root.as_deref(), callbacks)
                    .await
                    .expect("open durable record");
            let turns = session
                .list_turns()
                .await
                .expect("list turns")
                .iter()
                .map(|t| serde_json::to_value(t).expect("turn json"))
                .collect();
            let events = session
                .list_events()
                .await
                .expect("list events")
                .iter()
                .map(|e| serde_json::to_value(e).expect("event json"))
                .collect();
            (turns, events)
        })
    })
    .join()
    .expect("durable record thread")
}

/// F. In-run steer stays in the canonical task turn (turn parts, Flow 7;
/// correction M1). A real second `start_or_steer_turn` while the first host
/// turn is active is drained by `run_turn` after cycle 0 and recorded as a
/// steer prompt. The durable record must show: the opening prompt without a
/// steer assertion; the in-run prompt with `payload.steer = true`; one task
/// turn holding both prompts and every step-bearing member (no close/open
/// transition at the steer — the only closes are the prompt boundary that
/// opened the task turn and its `turn_end`); the host turn still bound to
/// that same durable turn; and the later MidTurn parts relief — driven by
/// sustained post-steer tool pressure under the SDK's production 120k lower
/// bound — splitting and serving that same turn, with no continuation turn
/// or forced-boundary marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_in_run_steer_stays_in_task_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    /// Heavy tool cycles after the steer (each ~32k chars of output) so the
    /// turn crosses the production lower bound and a parts split lands
    /// after the steer prompt.
    const POST_STEER_CYCLES: usize = 18;
    const SCOPE_LIMIT: i64 = 80_000;
    /// Wave seams where emulated usage approaches the scope limit.
    const WAVE_SEAMS: [usize; 3] = [6, 12, 17];

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();

    // Cycle 0: a tool call that holds the turn open long enough to steer it,
    // below the trigger so no relief runs before the steer is drained.
    let shell_args = json!({
        "cmd": "sleep 1; echo steer-window-open",
        "yield_time_ms": 10_000,
    })
    .to_string();
    let mut responses = vec![sse(vec![
        ev_response_created("resp-steer-0"),
        ev_function_call("call-steer-window", "exec_command", &shell_args),
        ev_completed_with_tokens("resp-steer-0", /*total_tokens*/ 300),
    ])];
    // Cycles 1..=N answer the drained steer and keep the task going under
    // sustained pressure; the parts arm runs at the wave seams.
    for i in 1..=POST_STEER_CYCLES {
        let usage: i64 = if WAVE_SEAMS.contains(&i) {
            78_400
        } else {
            40_000
        };
        let args = json!({
            "cmd": cycle_command(i),
            "yield_time_ms": 10_000,
        })
        .to_string();
        responses.push(sse(vec![
            ev_response_created(&format!("resp-steer-{i}")),
            ev_assistant_message(
                &format!("m-steer-{i}"),
                &format!("continuing with the steered direction, cycle {i}"),
            ),
            ev_function_call(&format!("call-steer-{i}"), "exec_command", &args),
            ev_completed_with_tokens(&format!("resp-steer-{i}"), usage),
        ]));
    }
    responses.push(sse(vec![
        ev_response_created("resp-steer-final"),
        ev_assistant_message(
            "m-steer-final",
            "task complete after steer and mid-turn parts compact",
        ),
        ev_completed_with_tokens("resp-steer-final", /*total_tokens*/ 300),
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
            // Auto-compact trigger only — not a body-size refuse. The parts
            // arm runs under the SDK's production lower bound (core's test
            // knobs are not compiled into integration builds).
            config.model_auto_compact_token_limit = Some(SCOPE_LIMIT);
            config.model_context_window = Some(400_000);
            config.model = Some("gpt-5.5".into());
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.into());
        });
    let test = builder.build(&server).await?;
    {
        let slot = test
            .codex
            .thread_extension_data()
            .get::<LhcCaptureSlot>()
            .expect("slot");
        let _ = wait_for_handle(&slot, Duration::from_secs(30)).await;
    }

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd_path.as_path());
    const OPENING: &str = "start the long agentic task";
    const STEER: &str = "steer: also cover the second half of the task";
    let started = test
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: OPENING.into(),
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
    let codex_protocol::turn_input::TurnInputSubmission::Started {
        turn_id: host_turn_id,
    } = started
    else {
        panic!("first submission must start the turn, got {started:?}");
    };
    // The turn is active (cycle 0's tool is executing): submit the real steer.
    wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::ExecCommandBegin(_))
    })
    .await;
    let steered = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: STEER.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let codex_protocol::turn_input::TurnInputSubmission::Steered {
        turn_id: steered_id,
    } = steered
    else {
        panic!("second submission must steer the active turn, got {steered:?}");
    };
    assert_eq!(
        steered_id, host_turn_id,
        "the steer joins the active host turn"
    );
    let terminal = wait_for_event(&test.codex, |ev| {
        matches!(
            ev,
            EventMsg::TurnComplete(_) | EventMsg::Error(_) | EventMsg::TurnAborted(_)
        )
    })
    .await;
    assert!(
        matches!(terminal, EventMsg::TurnComplete(_)),
        "steered turn must complete, got {terminal:?}"
    );

    let bodies = request_bodies(&mock);
    assert_eq!(
        bodies.len(),
        POST_STEER_CYCLES + 2,
        "cycle 0 (steer window), {POST_STEER_CYCLES} steered cycles, final completion; got {}",
        bodies.len()
    );
    assert!(
        !bodies.iter().any(|b| body_has_summarization(b)),
        "native compact must not run on the steered MidTurn path"
    );
    assert!(
        !bodies[0].contains(STEER) && bodies[1].contains(STEER),
        "the steer is drained into the request after the cycle it interrupted"
    );
    assert!(
        !bodies
            .iter()
            .any(|b| b.contains("lhc.compact_continuation")),
        "no request may carry the forced-boundary marker"
    );

    let slot = test
        .codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    handle.flush().await;
    let thread_id = handle.thread_id().to_string();
    let lhc_data_root = handle.root().map(std::path::Path::to_path_buf);

    // Durable record: prompts, steer assertion, single task turn.
    let messages = canonical_message_rows_blocking(&thread_id, lhc_data_root.clone());
    let prompt_rows: Vec<&(String, String)> = messages
        .iter()
        .filter(|(kind, _)| kind == "user_prompt")
        .collect();
    assert_eq!(
        prompt_rows.len(),
        2,
        "opening prompt and steer prompt are both recorded: {messages:?}"
    );
    let task_turn = prompt_rows[0].1.clone();
    assert_eq!(
        prompt_rows[1].1, task_turn,
        "the steer prompt is a member of the same durable turn as the opening prompt"
    );
    let step_kinds = [
        "assistant_text",
        "assistant_thinking",
        "tool_call",
        "tool_result",
    ];
    let foreign: Vec<&(String, String)> = messages
        .iter()
        .filter(|(kind, turn)| step_kinds.contains(&kind.as_str()) && *turn != task_turn)
        .collect();
    assert!(
        foreign.is_empty(),
        "every step-bearing member lives on the task turn {task_turn}: {foreign:?}"
    );
    assert_eq!(
        handle.durable_turn_id(&host_turn_id).as_deref(),
        Some(task_turn.as_str()),
        "the host turn stays bound to the original durable task turn across the steer"
    );

    let (turns, events) = durable_record_blocking(&thread_id, lhc_data_root.clone());
    let prompts: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["eventKind"] == "user_prompt")
        .collect();
    assert_eq!(prompts.len(), 2, "two user_prompt events: {events:?}");
    assert_eq!(prompts[0]["payload"]["text"], OPENING);
    assert!(
        prompts[0]["payload"]
            .get("steer")
            .is_none_or(|s| s == &json!(false)),
        "the opening prompt carries no steer assertion: {}",
        prompts[0]
    );
    assert_eq!(prompts[1]["payload"]["text"], STEER);
    assert_eq!(
        prompts[1]["payload"]["steer"],
        json!(true),
        "the in-run prompt is stamped steer=true: {}",
        prompts[1]
    );
    let opening_order = prompts[0]["eventOrder"].as_i64().expect("order");
    let steer_order = prompts[1]["eventOrder"].as_i64().expect("order");
    let turn_ends: Vec<i64> = events
        .iter()
        .filter(|e| e["eventKind"] == "turn_end")
        .map(|e| e["eventOrder"].as_i64().expect("order"))
        .collect();
    assert_eq!(turn_ends.len(), 1, "exactly one turn_end: {turn_ends:?}");
    // Canonical lifecycle: bootstrap turn closed by the opening prompt, the
    // task turn closed by turn_end, the SDK's empty successor. No close/open
    // at the steer.
    assert_eq!(
        turns.len(),
        3,
        "bootstrap, task and empty successor turns only (no continuation, no split at the steer): {turns:?}"
    );
    let task = turns
        .iter()
        .find(|t| t["turnId"] == task_turn)
        .unwrap_or_else(|| panic!("task turn {task_turn} in {turns:?}"));
    assert_eq!(task["status"], "closed");
    assert_eq!(task["outcome"], "completed");
    assert_eq!(task["openedAtEventOrder"].as_i64(), Some(opening_order));
    assert_eq!(task["closedAtEventOrder"].as_i64(), Some(turn_ends[0]));
    assert!(
        turns
            .iter()
            .all(|t| t["openedAtEventOrder"].as_i64() != Some(steer_order)
                && t["closedAtEventOrder"].as_i64() != Some(steer_order)),
        "no turn opens or closes at the steer prompt ({steer_order}): {turns:?}"
    );

    // Later MidTurn relief served the same durable turn as parts.
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, lhc_data_root.as_deref())
            .await
            .expect("inspect receipts");
    assert!(
        receipts.is_empty(),
        "steered turn never routes to compact-continuation: {receipts:?}"
    );
    let view = codex_lhc_host::inspect_installed_view(&thread_id, lhc_data_root.as_deref())
        .await
        .expect("describe")
        .expect("parts relief after the steer installs a serving view");
    assert!(
        codex_lhc_host::view_serves_parts(&view),
        "the task turn is served as parts: {view:?}"
    );
    assert!(
        view.arrangement
            .iter()
            .filter(|entry| entry.part.is_some())
            .all(|entry| entry.subject_id == task_turn),
        "parts relief addresses the same durable task turn {task_turn}: {view:?}"
    );
    assert!(
        bodies.iter().skip(2).any(|b| b.contains("[seam · ")),
        "a later request after the steer is served across the parts seam"
    );

    Ok(())
}

/// `(kind, turn_id)` for every live canonical message, in record order.
fn canonical_message_rows_blocking(
    thread_id: &str,
    root: Option<PathBuf>,
) -> Vec<(String, String)> {
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
                .map(|m| {
                    let v = serde_json::to_value(m).expect("message json");
                    (
                        v["kind"].as_str().unwrap_or_default().to_string(),
                        v["turnId"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        })
    })
    .join()
    .expect("canonical rows thread")
}
