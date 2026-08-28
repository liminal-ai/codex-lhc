//! LIM-134 production-path proofs (mapped).
//!
//! Natural gpt-5.6-sol 350K resumed PreTurn: wait-entry is the lifecycle
//! watch receiver-count increment, not `Opening` or elapsed time.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStopInput;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_lhc_host::CAPTURE_OPEN_FAILED;
use codex_lhc_host::CaptureState;
use codex_lhc_host::LateBoundCallbacks;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::install_with_root;
use codex_lhc_host::install_with_root_held_open;
use codex_lhc_host::lhc_inference_callbacks;
use codex_lhc_host::spawn_capture;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

const MODEL: &str = "gpt-5.6-sol";
const OVER_LIMIT_TOKENS: i64 = 351_000;
const SEED_PROMPT: &str = "LIM134_SEED_OVER_LIMIT";
const NEW_PROMPT: &str = "LIM134_RESUMED_USER_PROMPT";
const NO_TOOL_PROMPT: &str = "LIM134_NO_TOOL_PROMPT";
const WAITER_CEILING: Duration = Duration::from_secs(15);
const TURN_CEILING: Duration = Duration::from_secs(45);

fn non_openai_model_provider(server: &MockServer) -> ModelProviderInfo {
    let mut provider = built_in_model_providers(/* openai_base_url */ None)["openai"].clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.name = "MockProvider".into();
    provider.supports_websockets = false;
    provider.stream_max_retries = Some(0);
    provider.request_max_retries = Some(0);
    provider
}

fn lhc_ready(root: PathBuf) -> Arc<codex_extension_api::ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    install_with_root(&mut builder, |_c| true, root);
    Arc::new(builder.build())
}

fn lhc_held_open(root: PathBuf) -> Arc<codex_extension_api::ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    install_with_root_held_open(&mut builder, |_c| true, root);
    Arc::new(builder.build())
}

/// Records which turn-terminal contributor methods fired (LIM-134 F2).
struct TerminalRecorder {
    calls: Mutex<Vec<&'static str>>,
}

impl TerminalRecorder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
        })
    }

    fn snapshot(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl TurnLifecycleContributor for TerminalRecorder {
    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _ = input;
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("stop");
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _ = input;
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("abort");
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _ = input;
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("error");
        })
    }
}

fn lhc_ready_with_recorder(
    root: PathBuf,
    recorder: Arc<TerminalRecorder>,
) -> Arc<codex_extension_api::ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    install_with_root(&mut builder, |_c| true, root);
    builder.turn_lifecycle_contributor(recorder);
    Arc::new(builder.build())
}

fn lhc_held_open_with_recorder(
    root: PathBuf,
    recorder: Arc<TerminalRecorder>,
) -> Arc<codex_extension_api::ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    install_with_root_held_open(&mut builder, |_c| true, root);
    builder.turn_lifecycle_contributor(recorder);
    Arc::new(builder.build())
}

fn assert_exactly_one_terminal(calls: &[&str], expected: &str) {
    assert_eq!(
        calls,
        &[expected],
        "exactly one of stop/abort/error must fire; expected {expected}"
    );
}

fn apply_gpt56_lhc_config(config: &mut Config, model_provider: ModelProviderInfo) {
    config.model_provider = model_provider;
    let _ = config.features.enable(Feature::LhcCapture);
    let _ = config.features.disable(Feature::TokenBudget);
}

fn user_turn(text: &str) -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }])
}

fn prompt_body_match(prompt: &'static str) -> impl wiremock::Match + Send + Sync + 'static {
    move |req: &wiremock::Request| {
        let body = std::str::from_utf8(&req.body).unwrap_or("");
        body.contains(prompt)
    }
}

fn native_prompt_count(rollout: &Path, needle: &str) -> usize {
    let Ok(text) = std::fs::read_to_string(rollout) else {
        return 0;
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<RolloutLine>(line).ok())
        .filter(|line| match &line.item {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::Message { role, content, .. } if role == "user" => content
                    .iter()
                    .any(|item| matches!(item, ContentItem::InputText { text } if text == needle)),
                _ => false,
            },
            _ => false,
        })
        .count()
}

fn is_responses_post(req: &wiremock::Request) -> bool {
    req.method == "POST" && req.url.path().contains("/responses")
}

fn request_has_prompt(req: &wiremock::Request, prompt: &str) -> bool {
    std::str::from_utf8(&req.body).is_ok_and(|body| body.contains(prompt))
}

async fn responses_posts(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(is_responses_post)
        .collect()
}

fn prompt_bearing_count(posts: &[wiremock::Request], prompt: &str) -> usize {
    posts
        .iter()
        .filter(|req| request_has_prompt(req, prompt))
        .count()
}

struct Terminal {
    errors: usize,
    completes: usize,
    complete_has_error: bool,
    aborts: usize,
}

async fn wait_terminal(codex: &codex_core::CodexThread) -> Terminal {
    let mut terminal = Terminal {
        errors: 0,
        completes: 0,
        complete_has_error: false,
        aborts: 0,
    };
    let mut shutdown_submitted = false;
    loop {
        let ev = timeout(TURN_CEILING, codex.next_event())
            .await
            .expect("timeout waiting for terminal or shutdown")
            .expect("event stream ended");
        match &ev.msg {
            EventMsg::Error(_) => terminal.errors += 1,
            EventMsg::TurnComplete(complete) => {
                terminal.completes += 1;
                terminal.complete_has_error = complete.error.is_some();
            }
            EventMsg::TurnAborted(_) => terminal.aborts += 1,
            EventMsg::ShutdownComplete => return terminal,
            _ => {}
        }
        if !shutdown_submitted
            && matches!(ev.msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
        {
            shutdown_submitted = true;
            codex
                .submit(Op::Shutdown)
                .await
                .expect("submit shutdown after first terminal");
        }
    }
}

async fn wait_for_waiter_increment(slot: &LhcCaptureSlot, baseline: usize) {
    timeout(WAITER_CEILING, async {
        loop {
            if slot.readiness_waiter_count_for_test() == baseline + 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("deadlock ceiling: required compact did not subscribe");
}

async fn seed_over_limit_thread(server: &MockServer, lhc_root: PathBuf) -> Result<TestCodex> {
    let _seed = mount_sse_once_match(
        server,
        prompt_body_match(SEED_PROMPT),
        sse(vec![
            ev_assistant_message("seed-msg", "seed reply"),
            ev_completed_with_tokens("seed-resp", OVER_LIMIT_TOKENS),
        ]),
    )
    .await;
    let model_provider = non_openai_model_provider(server);
    let mut builder = test_codex()
        .with_model(MODEL)
        .with_extensions(lhc_ready(lhc_root))
        .with_config(move |config| apply_gpt56_lhc_config(config, model_provider));
    let seed = builder.build(server).await?;
    seed.codex
        .start_or_steer_turn(user_turn(SEED_PROMPT))
        .await?;
    wait_for_event_with_timeout(
        &seed.codex,
        |ev| matches!(ev, EventMsg::TurnComplete(_)),
        TURN_CEILING,
    )
    .await;
    Ok(seed)
}

async fn resume_held_open(
    server: &MockServer,
    seed: &TestCodex,
    lhc_root: PathBuf,
) -> Result<TestCodex> {
    resume_with_extensions(server, seed, lhc_held_open(lhc_root)).await
}

async fn resume_with_extensions(
    server: &MockServer,
    seed: &TestCodex,
    extensions: Arc<codex_extension_api::ExtensionRegistry<Config>>,
) -> Result<TestCodex> {
    let model_provider = non_openai_model_provider(server);
    let mut builder = test_codex()
        .with_model(MODEL)
        .with_extensions(extensions)
        .with_config(move |config| apply_gpt56_lhc_config(config, model_provider));
    builder.restart(server, seed).await
}

fn slot_of(test: &TestCodex) -> Arc<LhcCaptureSlot> {
    test.codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("LhcCaptureSlot")
}

async fn prove_wait_entry(test: &TestCodex, server: &MockServer) -> Arc<LhcCaptureSlot> {
    let slot = slot_of(test);
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "held-open installer must leave the slot Opening before submit"
    );
    let baseline = slot.readiness_waiter_count_for_test();
    let posts_before = responses_posts(server).await.len();
    test.codex
        .start_or_steer_turn(user_turn(NEW_PROMPT))
        .await
        .expect("submit resumed turn");
    wait_for_event_with_timeout(
        &test.codex,
        |ev| matches!(ev, EventMsg::TurnStarted(_)),
        TURN_CEILING,
    )
    .await;
    wait_for_waiter_increment(&slot, baseline).await;
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "waiter armed while still Opening"
    );
    let posts = responses_posts(server).await;
    assert_eq!(
        posts.len(),
        posts_before,
        "no provider request of any kind before Ready"
    );
    assert_eq!(prompt_bearing_count(&posts, NEW_PROMPT), 0);
    slot
}

/// Arm the required-compact waiter. XOR tests use this instead of
/// [`prove_wait_entry`] so they do not depend on the zero-request snapshot.
async fn arm_held_open_waiter(test: &TestCodex) -> Arc<LhcCaptureSlot> {
    let slot = slot_of(test);
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "held-open installer must leave the slot Opening before submit"
    );
    let baseline = slot.readiness_waiter_count_for_test();
    test.codex
        .start_or_steer_turn(user_turn(NEW_PROMPT))
        .await
        .expect("submit resumed turn");
    wait_for_event_with_timeout(
        &test.codex,
        |ev| matches!(ev, EventMsg::TurnStarted(_)),
        TURN_CEILING,
    )
    .await;
    wait_for_waiter_increment(&slot, baseline).await;
    assert!(
        matches!(slot.state(), CaptureState::Opening),
        "waiter armed while still Opening"
    );
    slot
}

async fn publish_ready(test: &TestCodex, slot: &LhcCaptureSlot, root: PathBuf) {
    let thread_id = test.session_configured.thread_id.to_string();
    let derivation = LateBoundCallbacks::new();
    derivation.seed(lhc_inference_callbacks(false).expect("deterministic callbacks"));
    let handle = spawn_capture(&thread_id, None, Some(root), derivation)
        .await
        .expect("open capture for Ready");
    assert!(
        slot.publish_ready_for_test(handle),
        "Ready must publish onto Opening"
    );
}

fn rollout_of(test: &TestCodex) -> PathBuf {
    test.session_configured
        .rollout_path
        .clone()
        .expect("rollout path")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn natural_350k_resumed_ready_orders_one_prompt_bearing_agent_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();
    let seed = seed_over_limit_thread(&server, root.clone()).await?;
    let resumed = resume_held_open(&server, &seed, root.clone()).await?;
    let _agent = mount_sse_once_match(
        &server,
        prompt_body_match(NEW_PROMPT),
        sse(vec![
            ev_assistant_message("follow-msg", "follow reply"),
            ev_completed("follow-resp"),
        ]),
    )
    .await;

    let slot = prove_wait_entry(&resumed, &server).await;
    publish_ready(&resumed, &slot, root).await;
    let terminal = wait_terminal(&resumed.codex).await;
    let posts = responses_posts(&server).await;

    assert_eq!(terminal.errors, 0);
    assert_eq!(terminal.aborts, 0);
    assert_eq!(terminal.completes, 1);
    assert!(!terminal.complete_has_error);
    assert_eq!(prompt_bearing_count(&posts, NEW_PROMPT), 1);
    assert_eq!(native_prompt_count(&rollout_of(&resumed), NEW_PROMPT), 1);
    Ok(())
}

#[derive(Clone, Copy)]
enum PreDispatch {
    Failed,
    Stopped,
    Interrupt,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_dispatch_failed_stopped_interrupt_after_waiter_proof() -> Result<()> {
    skip_if_no_network!(Ok(()));

    for row in [
        PreDispatch::Failed,
        PreDispatch::Stopped,
        PreDispatch::Interrupt,
    ] {
        let server = start_mock_server().await;
        let lhc_root = TempDir::new()?;
        let root = lhc_root.path().to_path_buf();
        let seed = seed_over_limit_thread(&server, root.clone()).await?;
        let resumed = resume_held_open(&server, &seed, root).await?;
        let slot = prove_wait_entry(&resumed, &server).await;
        let posts_at_wait = responses_posts(&server).await.len();
        match row {
            PreDispatch::Failed => {
                assert!(slot.publish_failed_for_test(CAPTURE_OPEN_FAILED));
            }
            PreDispatch::Stopped => slot.publish_stopped_for_test(),
            PreDispatch::Interrupt => {
                resumed.codex.submit(Op::Interrupt).await?;
            }
        }
        let terminal = wait_terminal(&resumed.codex).await;
        let posts = responses_posts(&server).await;
        assert_eq!(
            posts.len(),
            posts_at_wait,
            "pre-dispatch must not issue provider requests"
        );
        assert_eq!(prompt_bearing_count(&posts, NEW_PROMPT), 0);
        assert_eq!(native_prompt_count(&rollout_of(&resumed), NEW_PROMPT), 1);
        match row {
            PreDispatch::Failed | PreDispatch::Stopped => {
                assert_eq!(terminal.aborts, 0);
                assert_eq!(terminal.completes, 1);
                assert!(terminal.complete_has_error);
                assert_eq!(terminal.errors, 1);
            }
            PreDispatch::Interrupt => {
                assert_eq!(terminal.errors, 0);
                assert_eq!(terminal.aborts, 1);
                assert_eq!(terminal.completes, 0);
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_dispatch_prompt_bearing_agent_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();
    let seed = seed_over_limit_thread(&server, root.clone()).await?;
    let resumed = resume_held_open(&server, &seed, root.clone()).await?;
    let _agent = mount_sse_once_match(
        &server,
        prompt_body_match(NEW_PROMPT),
        sse_failed("agent-fail", "server_error", "forced agent failure"),
    )
    .await;

    let slot = prove_wait_entry(&resumed, &server).await;
    let posts_at_wait = responses_posts(&server).await.len();
    publish_ready(&resumed, &slot, root).await;
    let terminal = wait_terminal(&resumed.codex).await;
    let posts = responses_posts(&server).await;

    assert!(
        posts.len() > posts_at_wait,
        "post-dispatch failure must not claim zero provider requests"
    );
    assert_eq!(prompt_bearing_count(&posts, NEW_PROMPT), 1);
    assert_eq!(native_prompt_count(&rollout_of(&resumed), NEW_PROMPT), 1);
    assert_eq!(terminal.aborts, 0);
    assert_eq!(terminal.completes, 1);
    assert!(terminal.complete_has_error);
    assert_eq!(terminal.errors, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_tool_success_is_one_provider_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let _agent = mount_sse_once_match(
        &server,
        prompt_body_match(NO_TOOL_PROMPT),
        sse(vec![
            ev_assistant_message("ok-msg", "ok reply"),
            ev_completed("ok-resp"),
        ]),
    )
    .await;
    let model_provider = non_openai_model_provider(&server);
    let mut builder = test_codex()
        .with_model(MODEL)
        .with_extensions(lhc_ready(lhc_root.path().to_path_buf()))
        .with_config(move |config| apply_gpt56_lhc_config(config, model_provider));
    let test = builder.build(&server).await?;
    test.codex
        .start_or_steer_turn(user_turn(NO_TOOL_PROMPT))
        .await?;
    let terminal = wait_terminal(&test.codex).await;
    let posts = responses_posts(&server).await;

    assert_eq!(posts.len(), 1);
    assert_eq!(prompt_bearing_count(&posts, NO_TOOL_PROMPT), 1);
    assert_eq!(native_prompt_count(&rollout_of(&test), NO_TOOL_PROMPT), 1);
    assert_eq!(terminal.errors, 0);
    assert_eq!(terminal.aborts, 0);
    assert_eq!(terminal.completes, 1);
    assert!(!terminal.complete_has_error);
    Ok(())
}

// ---------------------------------------------------------------------------
// LIM-134 F2: contributor-level exactly-one terminal
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contributor_xor_normal_success_fires_only_stop() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let recorder = TerminalRecorder::new();
    let _agent = mount_sse_once_match(
        &server,
        prompt_body_match(NO_TOOL_PROMPT),
        sse(vec![
            ev_assistant_message("ok-msg", "ok reply"),
            ev_completed("ok-resp"),
        ]),
    )
    .await;
    let model_provider = non_openai_model_provider(&server);
    let mut builder = test_codex()
        .with_model(MODEL)
        .with_extensions(lhc_ready_with_recorder(
            lhc_root.path().to_path_buf(),
            Arc::clone(&recorder),
        ))
        .with_config(move |config| apply_gpt56_lhc_config(config, model_provider));
    let test = builder.build(&server).await?;
    test.codex
        .start_or_steer_turn(user_turn(NO_TOOL_PROMPT))
        .await?;
    let terminal = wait_terminal(&test.codex).await;
    assert_eq!(terminal.errors, 0);
    assert_eq!(terminal.aborts, 0);
    assert_eq!(terminal.completes, 1);
    assert!(!terminal.complete_has_error);
    assert_exactly_one_terminal(&recorder.snapshot(), "stop");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contributor_xor_pre_dispatch_required_compact_failure_fires_only_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();
    let recorder = TerminalRecorder::new();
    let seed = seed_over_limit_thread(&server, root.clone()).await?;
    let resumed = resume_with_extensions(
        &server,
        &seed,
        lhc_held_open_with_recorder(root, Arc::clone(&recorder)),
    )
    .await?;
    let slot = arm_held_open_waiter(&resumed).await;
    assert!(slot.publish_failed_for_test(CAPTURE_OPEN_FAILED));
    let terminal = wait_terminal(&resumed.codex).await;
    assert_eq!(terminal.aborts, 0);
    assert_eq!(terminal.completes, 1);
    assert!(terminal.complete_has_error);
    assert_eq!(terminal.errors, 1);
    assert_exactly_one_terminal(&recorder.snapshot(), "error");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contributor_xor_post_dispatch_sampling_failure_fires_only_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let recorder = TerminalRecorder::new();
    let _agent = mount_sse_once_match(
        &server,
        prompt_body_match(NO_TOOL_PROMPT),
        sse_failed("agent-fail", "server_error", "forced agent failure"),
    )
    .await;
    let model_provider = non_openai_model_provider(&server);
    let mut builder = test_codex()
        .with_model(MODEL)
        .with_extensions(lhc_ready_with_recorder(
            lhc_root.path().to_path_buf(),
            Arc::clone(&recorder),
        ))
        .with_config(move |config| apply_gpt56_lhc_config(config, model_provider));
    let test = builder.build(&server).await?;
    test.codex
        .start_or_steer_turn(user_turn(NO_TOOL_PROMPT))
        .await?;
    let terminal = wait_terminal(&test.codex).await;
    assert_eq!(terminal.aborts, 0);
    assert_eq!(terminal.completes, 1);
    assert!(terminal.complete_has_error);
    assert_eq!(terminal.errors, 1);
    assert_exactly_one_terminal(&recorder.snapshot(), "error");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contributor_xor_interrupt_fires_only_abort() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();
    let recorder = TerminalRecorder::new();
    let seed = seed_over_limit_thread(&server, root.clone()).await?;
    let resumed = resume_with_extensions(
        &server,
        &seed,
        lhc_held_open_with_recorder(root, Arc::clone(&recorder)),
    )
    .await?;
    let _slot = arm_held_open_waiter(&resumed).await;
    resumed.codex.submit(Op::Interrupt).await?;
    let terminal = wait_terminal(&resumed.codex).await;
    assert_eq!(terminal.errors, 0);
    assert_eq!(terminal.aborts, 1);
    assert_eq!(terminal.completes, 0);
    assert_exactly_one_terminal(&recorder.snapshot(), "abort");
    Ok(())
}
