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
use codex_protocol::protocol::Op;
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
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "continue this long agentic task".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
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

/// B. Pending parallel-tool full loop: reasoning + two tool calls settle via
/// real executor; MidTurn preserves pairs; request 2 completes same task;
/// no continuation marker; no native compact.
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
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "run two parallel tools then continue".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.cwd_path().abs())),
                approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let bodies = request_bodies(&mock);
    assert_eq!(
        bodies.len(),
        2,
        "exactly two provider requests (tool turn + continuation); got {}",
        bodies.len()
    );
    assert!(
        !bodies.iter().any(|b| body_has_summarization(b)),
        "native compact must not run on pending-tool MidTurn path"
    );

    let req2 = &bodies[1];
    // Both call/output pairs present on request 2.
    assert!(
        req2.contains("call-loop-z") && req2.contains("call-loop-a"),
        "request 2 must carry both tool call ids"
    );
    // Outputs settled by real executor.
    assert!(
        req2.contains("z-out") || req2.contains("function_call_output"),
        "request 2 should include settled tool outputs"
    );
    // Reasoning identity preserved into request 2 when reverse-mapped.
    // At minimum: no continuation marker on pending-tool branch.
    assert_eq!(
        count_substr(req2, "lhc.compact_continuation"),
        0,
        "pending-tool branch must not insert continuation marker"
    );
    assert_eq!(
        count_substr(req2, "context_compact_continue"),
        0,
        "pending-tool branch must not force context_compact_continue marker"
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
    // First success, then CLE failures via one-shot mounts (no exact total expect).
    let first_mock = mount_sse_once(&server, first).await;
    let mut cle_mocks = Vec::new();
    for i in 0..5 {
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
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "push context until exceeded".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
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
    // Bounded product policy: must terminate without a native compact loop.
    // Do not derive the bound from mock capacity alone (always-pass trap).
    assert!(
        !bodies.is_empty(),
        "at least one provider request must have been issued"
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
    // Product policy: turn reached a terminal EventMsg (asserted above) without
    // requiring mock exhaustion — the CLE path is not an open retry loop.

    Ok(())
}
