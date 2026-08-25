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
    // Slice C counts serialized JSON with o200k, not chars/4. 80k still
    // triggers at the wave seams while leaving room for the accurate estimate.
    const SCOPE_LIMIT: i64 = 80_000;
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
            78_400
        } else {
            40_000
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

    // Durable mechanism evidence (turn parts): the sustained relief split the
    // active turn into parts inside the one Codex turn — the installed view
    // serves parts, no continuation turn was opened, and no request carried
    // the forced-boundary marker.
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

    Ok(())
}
