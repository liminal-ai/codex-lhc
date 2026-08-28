use super::*;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::TraceId;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use pretty_assertions::assert_eq;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::tempdir;
use tracing_opentelemetry::OpenTelemetrySpanExt;

fn test_tracing_subscriber() -> impl tracing::Subscriber + Send + Sync {
    let provider = SdkTracerProvider::builder().build();
    let tracer = provider.tracer("codex-exec-tests");
    tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer))
}

#[derive(Clone)]
struct TestLogWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

struct TestLogSink {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TestLogWriter {
    type Writer = TestLogSink;

    fn make_writer(&'a self) -> Self::Writer {
        TestLogSink {
            buffer: Arc::clone(&self.buffer),
        }
    }
}

impl Write for TestLogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().expect("log buffer lock").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn exec_default_stderr_filter_suppresses_otel_self_diagnostics() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = TestLogWriter {
        buffer: Arc::clone(&buffer),
    };
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(EnvFilter::try_new(EXEC_DEFAULT_LOG_FILTER).expect("default filter")),
    );

    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(target: "opentelemetry_sdk", "telemetry export failed");
        tracing::error!(target: "opentelemetry_otlp", "telemetry request failed");
        tracing::error!(target: "codex_exec_test", "real exec error");
    });

    let logs = String::from_utf8(buffer.lock().expect("log buffer lock").clone()).expect("utf8");
    assert!(!logs.contains("telemetry export failed"));
    assert!(!logs.contains("telemetry request failed"));
    assert!(logs.contains("real exec error"));
}

#[test]
fn exec_root_span_can_be_parented_from_trace_context() {
    let subscriber = test_tracing_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let parent = codex_protocol::protocol::W3cTraceContext {
        traceparent: Some("00-00000000000000000000000000000077-0000000000000088-01".into()),
        tracestate: Some("vendor=value".into()),
    };
    let exec_span = exec_root_span();
    assert!(set_parent_from_w3c_trace_context(&exec_span, &parent));

    let trace_id = exec_span.context().span().span_context().trace_id();
    assert_eq!(
        trace_id,
        TraceId::from_hex("00000000000000000000000000000077").expect("trace id")
    );
}

#[test]
fn builds_uncommitted_review_request() {
    let args = ReviewArgs {
        uncommitted: true,
        base: None,
        commit: None,
        commit_title: None,
        prompt: None,
    };
    let request = build_review_request(&args).expect("builds uncommitted review request");

    let expected = ReviewRequest {
        target: ReviewTarget::UncommittedChanges,
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn builds_commit_review_request_with_title() {
    let args = ReviewArgs {
        uncommitted: false,
        base: None,
        commit: Some("123456789".to_string()),
        commit_title: Some("Add review command".to_string()),
        prompt: None,
    };
    let request = build_review_request(&args).expect("builds commit review request");

    let expected = ReviewRequest {
        target: ReviewTarget::Commit {
            sha: "123456789".to_string(),
            title: Some("Add review command".to_string()),
        },
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn builds_custom_review_request_trims_prompt() {
    let args = ReviewArgs {
        uncommitted: false,
        base: None,
        commit: None,
        commit_title: None,
        prompt: Some("  custom review instructions  ".to_string()),
    };
    let request = build_review_request(&args).expect("builds custom review request");

    let expected = ReviewRequest {
        target: ReviewTarget::Custom {
            instructions: "custom review instructions".to_string(),
        },
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn decode_prompt_bytes_strips_utf8_bom() {
    let input = [0xEF, 0xBB, 0xBF, b'h', b'i', b'\n'];

    let out = decode_prompt_bytes(&input).expect("decode utf-8 with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_decodes_utf16le_bom() {
    // UTF-16LE BOM + "hi\n"
    let input = [0xFF, 0xFE, b'h', 0x00, b'i', 0x00, b'\n', 0x00];

    let out = decode_prompt_bytes(&input).expect("decode utf-16le with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_decodes_utf16be_bom() {
    // UTF-16BE BOM + "hi\n"
    let input = [0xFE, 0xFF, 0x00, b'h', 0x00, b'i', 0x00, b'\n'];

    let out = decode_prompt_bytes(&input).expect("decode utf-16be with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_rejects_utf32le_bom() {
    // UTF-32LE BOM + "hi\n"
    let input = [
        0xFF, 0xFE, 0x00, 0x00, b'h', 0x00, 0x00, 0x00, b'i', 0x00, 0x00, 0x00, b'\n', 0x00, 0x00,
        0x00,
    ];

    let err = decode_prompt_bytes(&input).expect_err("utf-32le should be rejected");

    assert_eq!(
        err,
        PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32LE"
        }
    );
}

#[test]
fn decode_prompt_bytes_rejects_utf32be_bom() {
    // UTF-32BE BOM + "hi\n"
    let input = [
        0x00, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x00, b'h', 0x00, 0x00, 0x00, b'i', 0x00, 0x00, 0x00,
        b'\n',
    ];

    let err = decode_prompt_bytes(&input).expect_err("utf-32be should be rejected");

    assert_eq!(
        err,
        PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32BE"
        }
    );
}

#[test]
fn decode_prompt_bytes_rejects_invalid_utf8() {
    // Invalid UTF-8 sequence: 0xC3 0x28
    let input = [0xC3, 0x28];

    let err = decode_prompt_bytes(&input).expect_err("invalid utf-8 should fail");

    assert_eq!(err, PromptDecodeError::InvalidUtf8 { valid_up_to: 0 });
}

#[test]
fn prompt_with_stdin_context_wraps_stdin_block() {
    let combined = prompt_with_stdin_context("Summarize this concisely", "my output");

    assert_eq!(
        combined,
        "Summarize this concisely\n\n<stdin>\nmy output\n</stdin>"
    );
}

#[test]
fn prompt_with_stdin_context_preserves_trailing_newline() {
    let combined = prompt_with_stdin_context("Summarize this concisely", "my output\n");

    assert_eq!(
        combined,
        "Summarize this concisely\n\n<stdin>\nmy output\n</stdin>"
    );
}

#[test]
fn lagged_event_warning_message_is_explicit() {
    assert_eq!(
        lagged_event_warning_message(/*skipped*/ 7),
        "in-process app-server event stream lagged; dropped 7 events".to_string()
    );
}

#[test]
fn runtime_warnings_are_filtered_to_the_primary_thread() {
    let primary_thread_id = "thread-1";
    let turn_id = "turn-1";
    let outcomes = [
        codex_app_server_protocol::WarningNotification {
            thread_id: None,
            message: "global warning".to_string(),
        },
        codex_app_server_protocol::WarningNotification {
            thread_id: Some(primary_thread_id.to_string()),
            message: "primary warning".to_string(),
        },
        codex_app_server_protocol::WarningNotification {
            thread_id: Some("thread-2".to_string()),
            message: "other warning".to_string(),
        },
    ]
    .map(|warning| {
        should_process_notification(
            &ServerNotification::Warning(warning),
            primary_thread_id,
            turn_id,
        )
    });

    assert_eq!(outcomes, [true, true, false]);
}

#[tokio::test]
async fn resume_lookup_model_providers_filters_only_last_lookup() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build default config");
    config.model_provider_id = "test-provider".to_string();

    let last_args = crate::cli::ResumeArgs {
        session_id: None,
        last: true,
        all: false,
        images: vec![],
        prompt: None,
    };
    let named_args = crate::cli::ResumeArgs {
        session_id: Some("named-session".to_string()),
        last: false,
        all: false,
        images: vec![],
        prompt: None,
    };

    assert_eq!(
        resume_lookup_model_providers(&config, &last_args),
        Some(vec!["test-provider".to_string()])
    );
    assert_eq!(resume_lookup_model_providers(&config, &named_args), None);
}

#[test]
fn turn_items_for_thread_returns_matching_turn_items() {
    let thread = AppServerThread {
        id: "thread-1".to_string(),
        extra: None,
        session_id: "thread-1".to_string(),
        forked_from_id: None,
        parent_thread_id: None,
        preview: String::new(),
        ephemeral: false,
        section: None,
        section_entered_at: None,
        project_id: None,
        history_mode: Default::default(),
        model_provider: "openai".to_string(),
        created_at: 0,
        updated_at: 0,
        recency_at: Some(0),
        status: codex_app_server_protocol::ThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/tmp/project").abs(),
        cli_version: "0.0.0-test".to_string(),
        source: codex_app_server_protocol::SessionSource::Exec,
        can_accept_direct_input: None,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        turns: vec![
            codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![AppServerThreadItem::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "hello".to_string(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                }],
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
            codex_app_server_protocol::Turn {
                id: "turn-2".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![AppServerThreadItem::Plan {
                    id: "plan-1".to_string(),
                    text: "ship it".to_string(),
                }],
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        ],
    };

    assert_eq!(
        turn_items_for_thread(&thread, "turn-1"),
        Some(vec![AppServerThreadItem::AgentMessage {
            id: "msg-1".to_string(),
            text: "hello".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
        }])
    );
    assert_eq!(turn_items_for_thread(&thread, "missing-turn"), None);
}

#[test]
fn should_backfill_turn_completed_items_backfills_persisted_summaries_only() {
    let notification =
        ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Summary,
                items: Vec::new(),
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        });

    assert!(!should_backfill_turn_completed_items(
        /*thread_ephemeral*/ true,
        &notification
    ));
    assert!(should_backfill_turn_completed_items(
        /*thread_ephemeral*/ false,
        &notification
    ));
}

#[test]
fn canceled_mcp_server_elicitation_response_uses_cancel_action() {
    let value = canceled_mcp_server_elicitation_response()
        .expect("mcp elicitation cancel response should serialize");
    let response: McpServerElicitationRequestResponse =
        serde_json::from_value(value).expect("cancel response should deserialize");

    assert_eq!(
        response,
        McpServerElicitationRequestResponse {
            action: McpServerElicitationAction::Cancel,
            content: None,
            meta: None,
        }
    );
}

#[tokio::test]
async fn thread_start_params_include_review_policy_when_review_policy_is_manual_only() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            approvals_reviewer: Some(ApprovalsReviewer::User),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with manual-only review policy");

    let params = thread_start_params_from_config(&config, &ThreadSource::User);

    assert_eq!(
        params.approvals_reviewer,
        Some(codex_app_server_protocol::ApprovalsReviewer::User)
    );
    assert_eq!(params.sandbox, None);
    assert_eq!(
        params.permissions,
        permissions_selection_from_config(&config)
    );
}

#[tokio::test]
async fn thread_start_params_include_review_policy_when_auto_review_is_enabled() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            approvals_reviewer: Some(ApprovalsReviewer::AutoReview),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with guardian review policy");

    let params = thread_start_params_from_config(&config, &ThreadSource::User);

    assert_eq!(
        params.approvals_reviewer,
        Some(codex_app_server_protocol::ApprovalsReviewer::AutoReview)
    );
}

#[tokio::test]
async fn thread_resume_params_only_include_explicit_review_policy_override() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            approvals_reviewer: Some(ApprovalsReviewer::AutoReview),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with guardian review policy");

    let params_without_override = thread_resume_params_from_config(
        &config,
        "thread-id".to_string(),
        /*approvals_reviewer_override*/ None,
    );
    let params_with_override = thread_resume_params_from_config(
        &config,
        "thread-id".to_string(),
        Some(codex_app_server_protocol::ApprovalsReviewer::AutoReview),
    );

    assert_eq!(params_without_override.approvals_reviewer, None);
    assert_eq!(
        params_with_override.approvals_reviewer,
        Some(codex_app_server_protocol::ApprovalsReviewer::AutoReview)
    );
}

#[tokio::test]
async fn build_exec_config_retries_without_invalid_headless_policy_for_auto_review() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
approval_policy = "on-request"
approvals_reviewer = "auto_review"
"#,
    )
    .expect("write config");
    let requirements_path = codex_home.path().join("requirements.toml");
    std::fs::write(
        &requirements_path,
        r#"
allowed_approval_policies = ["never", "on-request"]
allowed_sandbox_modes = ["read-only", "workspace-write"]
"#,
    )
    .expect("write requirements");
    let mut loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    loader_overrides.system_requirements_path = Some(requirements_path);
    let overrides = ConfigOverrides {
        cwd: Some(cwd.path().to_path_buf()),
        approval_policy: Some(AskForApproval::Never),
        sandbox_mode: Some(SandboxMode::DangerFullAccess),
        ..Default::default()
    };
    let build_config = |overrides| {
        ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .loader_overrides(loader_overrides.clone())
            .harness_overrides(overrides)
            .build()
    };

    let error = build_config(overrides.clone())
        .await
        .expect_err("synthetic headless approval policy should fail");
    assert!(
        error
            .to_string()
            .contains("`approval_policy = \"never\"` cannot be used")
    );

    let config = build_exec_config(
        overrides,
        /*preserve_headless_approval_policy*/ false,
        build_config,
    )
    .await
    .expect("auto-review config should retry without the synthetic approval policy");

    assert_eq!(
        config.permissions.approval_policy.value(),
        AskForApproval::OnRequest
    );
    assert_eq!(config.approvals_reviewer, ApprovalsReviewer::AutoReview);
}

#[tokio::test]
async fn build_exec_config_preserves_headless_error_when_retry_fails() {
    let overrides = ConfigOverrides {
        approval_policy: Some(AskForApproval::Never),
        ..Default::default()
    };

    let error = build_exec_config(
        overrides,
        /*preserve_headless_approval_policy*/ false,
        |overrides| async move {
            let message = if overrides.approval_policy == Some(AskForApproval::Never) {
                "headless error"
            } else {
                "retry error"
            };
            Err(std::io::Error::other(message))
        },
    )
    .await
    .expect_err("failed speculative retry should preserve the original error");

    assert_eq!(error.to_string(), "headless error");
}

#[tokio::test]
async fn thread_start_params_match_history_to_persistence() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");

    let params = thread_start_params_from_config(&config, &ThreadSource::User);

    assert_eq!(
        params.thread_source,
        Some(codex_app_server_protocol::ThreadSource::User)
    );
    assert_eq!(params.history_mode, Some(ThreadHistoryMode::Paginated));

    let thread_source = ThreadSource::Feature("automated_review".to_string());
    let params = thread_start_params_from_config(&config, &thread_source);
    assert_eq!(params.thread_source, Some(thread_source));

    config.ephemeral = true;
    let params = thread_start_params_from_config(&config, &ThreadSource::User);

    assert_eq!(params.ephemeral, Some(true));
    assert_eq!(params.history_mode, None);
}

#[tokio::test]
async fn thread_lifecycle_params_preserve_hook_trust_bypass() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            bypass_hook_trust: Some(true),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with hook trust bypass");
    let expected_config = Some(HashMap::from([(
        "bypass_hook_trust".to_string(),
        serde_json::Value::Bool(true),
    )]));

    let start_params = thread_start_params_from_config(&config, &ThreadSource::User);
    let resume_params = thread_resume_params_from_config(
        &config,
        "thread-id".to_string(),
        /*approvals_reviewer_override*/ None,
    );

    assert_eq!(start_params.config, expected_config);
    assert_eq!(resume_params.config, expected_config);
}

#[test]
fn active_profile_selection_uses_profile_id_only() {
    let selection = permission_profile_id_from_active_profile(ActivePermissionProfile::new(
        BUILT_IN_PERMISSION_PROFILE_WORKSPACE,
    ));

    assert_eq!(selection, BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string());
}

#[tokio::test]
async fn thread_lifecycle_params_include_legacy_sandbox_when_no_active_profile() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with legacy sandbox override");

    let start_params = thread_start_params_from_config(&config, &ThreadSource::User);
    let resume_params = thread_resume_params_from_config(
        &config,
        "thread-id".to_string(),
        /*approvals_reviewer_override*/ None,
    );

    assert_eq!(config.permissions.active_permission_profile(), None);
    assert_eq!(
        start_params.sandbox,
        Some(codex_app_server_protocol::SandboxMode::DangerFullAccess)
    );
    assert_eq!(start_params.permissions, None);
    assert_eq!(
        resume_params.sandbox,
        Some(codex_app_server_protocol::SandboxMode::DangerFullAccess)
    );
    assert_eq!(resume_params.permissions, None);
}

#[tokio::test]
async fn session_configured_from_thread_response_uses_review_policy_from_response() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let response = sample_thread_start_response();

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(
        event.session_id.to_string(),
        "67e55044-10b1-426f-9247-bb680e5fe0c7"
    );
    assert_eq!(
        event.thread_id.to_string(),
        "67e55044-10b1-426f-9247-bb680e5fe0c8"
    );
    assert_eq!(event.approvals_reviewer, ApprovalsReviewer::AutoReview);
}

#[tokio::test]
async fn session_configured_from_thread_response_uses_permission_profile_from_config() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let response = sample_thread_start_response();

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(
        event.permission_profile,
        config.permissions.effective_permission_profile()
    );
}

#[tokio::test]
async fn session_configured_from_thread_response_preserves_thread_source() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let response = sample_thread_start_response();

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(
        event.thread_source,
        Some(codex_protocol::protocol::ThreadSource::User)
    );
}

#[tokio::test]
async fn session_configured_from_thread_response_preserves_parent_thread_id() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let parent_thread_id = ThreadId::new();
    let forked_from_id = ThreadId::new();
    let mut response = sample_thread_start_response();
    response.thread.parent_thread_id = Some(parent_thread_id.to_string());
    response.thread.forked_from_id = Some(forked_from_id.to_string());

    let event = session_configured_from_thread_start_response(&response, &config)
        .expect("build bootstrap session configured event");

    assert_eq!(event.parent_thread_id, Some(parent_thread_id));
    assert_eq!(event.forked_from_id, Some(forked_from_id));
}

fn sample_thread_start_response() -> ThreadStartResponse {
    ThreadStartResponse {
        thread: codex_app_server_protocol::Thread {
            id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
            extra: None,
            session_id: "67e55044-10b1-426f-9247-bb680e5fe0c7".to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            preview: String::new(),
            ephemeral: false,
            section: None,
            section_entered_at: None,
            project_id: None,
            history_mode: Default::default(),
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at: 0,
            recency_at: Some(0),
            status: codex_app_server_protocol::ThreadStatus::Idle,
            path: Some(PathBuf::from("/tmp/rollout.jsonl")),
            cwd: test_path_buf("/tmp").abs(),
            cli_version: "0.0.0".to_string(),
            source: codex_app_server_protocol::SessionSource::Cli,
            can_accept_direct_input: None,
            thread_source: Some(codex_app_server_protocol::ThreadSource::User),
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: Some("thread".to_string()),
            turns: vec![],
        },
        model: "gpt-5.4".to_string(),
        model_provider: "openai".to_string(),
        service_tier: None,
        cwd: test_path_buf("/tmp").abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_sources: Vec::new(),
        approval_policy: codex_app_server_protocol::AskForApproval::OnRequest,
        approvals_reviewer: codex_app_server_protocol::ApprovalsReviewer::AutoReview,
        sandbox: codex_app_server_protocol::SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
        active_permission_profile: None,
        reasoning_effort: None,
        multi_agent_mode: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// LIM-134: exec direct-prompt empty-result truth
// ---------------------------------------------------------------------------

fn completed_turn(items: Vec<AppServerThreadItem>) -> ServerNotification {
    ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
        thread_id: "thread-1".to_string(),
        turn: codex_app_server_protocol::Turn {
            id: "turn-1".to_string(),
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            items,
            status: codex_app_server_protocol::TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: Some(0),
            duration_ms: None,
        },
    })
}

fn agent_message(id: &str, text: &str) -> AppServerThreadItem {
    AppServerThreadItem::AgentMessage {
        id: id.to_string(),
        text: text.to_string(),
        phase: None,
        memory_citation: None,
        delivery: None,
    }
}

fn streamed_agent_message(turn_id: &str, text: &str) -> ServerNotification {
    ServerNotification::ItemCompleted(codex_app_server_protocol::ItemCompletedNotification {
        item: agent_message("msg-streamed", text),
        thread_id: "thread-1".to_string(),
        turn_id: turn_id.to_string(),
        completed_at_ms: 0,
    })
}

fn turn_status(notification: &ServerNotification) -> codex_app_server_protocol::TurnStatus {
    let ServerNotification::TurnCompleted(payload) = notification else {
        panic!("expected a turn completion");
    };
    payload.turn.status.clone()
}

fn user_turn(items: Vec<UserInput>) -> InitialOperation {
    InitialOperation::UserTurn {
        items,
        output_schema: None,
    }
}

fn text(text: &str) -> UserInput {
    UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }
}

/// Every substantive direct input promises an answer, including non-text
/// inputs. Blank text and non-`UserTurn` operations do not.
#[test]
fn expects_agent_message_covers_substantive_direct_input_only() {
    assert!(expects_agent_message(&user_turn(vec![text(
        "do the thing"
    )])));
    assert!(
        expects_agent_message(&user_turn(vec![
            UserInput::LocalImage {
                path: std::path::PathBuf::from("/tmp/shot.png"),
                detail: None,
            },
            text("   "),
        ])),
        "an image with a blank caption is still work"
    );
    assert!(!expects_agent_message(&user_turn(vec![text("   \n\t ")])));
    assert!(!expects_agent_message(&user_turn(Vec::new())));
    assert!(!expects_agent_message(&InitialOperation::ForkOnly));
}

/// Absent, blank, and whitespace-only completions are not successful results
/// for a substantive direct prompt.
#[test]
fn completed_turn_without_an_agent_message_is_reclassified_as_failed() {
    let cases: Vec<(&str, Vec<AppServerThreadItem>)> = vec![
        ("absent", Vec::new()),
        ("empty", vec![agent_message("msg-1", "")]),
        ("whitespace", vec![agent_message("msg-1", "  \n\t ")]),
    ];
    for (label, items) in cases {
        let evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
        let mut notification = completed_turn(items);
        evidence.reclassify_empty_result(&mut notification);
        assert_eq!(
            turn_status(&notification),
            codex_app_server_protocol::TurnStatus::Failed,
            "{label} must not report a successful empty result"
        );
        let ServerNotification::TurnCompleted(payload) = &notification else {
            unreachable!()
        };
        assert!(
            payload.turn.error.is_some(),
            "{label} must carry a failure diagnostic"
        );
        assert!(
            payload.turn.items.is_empty(),
            "{label} must not carry a fabricated answer"
        );
    }
}

/// A completed turn whose only substantive output is a nonblank Plan is
/// answer evidence (LIM-134 F5) — matching both processors' Plan fallback.
#[test]
fn completed_turn_with_a_nonblank_plan_is_not_reclassified_as_failed() {
    let evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
    let mut notification = completed_turn(vec![AppServerThreadItem::Plan {
        id: "plan-1".to_string(),
        text: "a nonblank plan counts as answer evidence".to_string(),
    }]);
    evidence.reclassify_empty_result(&mut notification);
    assert_eq!(
        turn_status(&notification),
        codex_app_server_protocol::TurnStatus::Completed,
        "plan only must not be reclassified as failed"
    );
}

/// A backfilled nonblank current-Turn agent message remains a success.
#[test]
fn backfilled_agent_message_remains_a_successful_result() {
    let evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
    let mut notification = completed_turn(vec![agent_message("msg-1", "the answer")]);
    evidence.reclassify_empty_result(&mut notification);
    assert_eq!(
        turn_status(&notification),
        codex_app_server_protocol::TurnStatus::Completed
    );
}

/// A valid answer that streamed while completion items stayed empty (the
/// ephemeral / no-backfill shape) is still a success.
#[test]
fn streamed_agent_message_remains_a_successful_result_without_backfill() {
    let mut evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
    evidence.observe(&streamed_agent_message("turn-1", "the answer"));
    let mut notification = completed_turn(Vec::new());
    evidence.reclassify_empty_result(&mut notification);
    assert_eq!(
        turn_status(&notification),
        codex_app_server_protocol::TurnStatus::Completed,
        "streamed evidence must not be discarded when completion items are empty"
    );
}

/// A blank streamed message is not evidence of an answer.
#[test]
fn blank_streamed_agent_message_is_not_evidence() {
    let mut evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
    evidence.observe(&streamed_agent_message("turn-1", "   "));
    let mut notification = completed_turn(Vec::new());
    evidence.reclassify_empty_result(&mut notification);
    assert_eq!(
        turn_status(&notification),
        codex_app_server_protocol::TurnStatus::Failed
    );
}

/// Operations that never promised an answer — Review, fork-only, blank input —
/// are never reclassified.
#[test]
fn operations_without_a_substantive_prompt_are_never_reclassified() {
    let evidence = DirectTurnAnswerEvidence::new(/*expected*/ false);
    let mut notification = completed_turn(Vec::new());
    evidence.reclassify_empty_result(&mut notification);
    assert_eq!(
        turn_status(&notification),
        codex_app_server_protocol::TurnStatus::Completed
    );
}

/// Already-failed and interrupted completions keep their own status.
#[test]
fn non_completed_turn_status_is_left_alone() {
    let evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
    for status in [
        codex_app_server_protocol::TurnStatus::Failed,
        codex_app_server_protocol::TurnStatus::Interrupted,
        codex_app_server_protocol::TurnStatus::InProgress,
    ] {
        let mut notification = completed_turn(Vec::new());
        if let ServerNotification::TurnCompleted(payload) = &mut notification {
            payload.turn.status = status.clone();
        }
        evidence.reclassify_empty_result(&mut notification);
        assert_eq!(turn_status(&notification), status);
    }
}

/// LIM-134 F4: reclassified empty completion is consumed by the JSONL
/// processor as `turn.failed`, never `turn.completed`.
#[test]
fn reclassified_empty_result_jsonl_emits_turn_failed() {
    let evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
    let mut notification = completed_turn(Vec::new());
    evidence.reclassify_empty_result(&mut notification);

    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);
    let collected = processor.collect_thread_events(notification);
    assert_eq!(collected.status, CodexStatus::InitiateShutdown);
    assert!(
        collected
            .events
            .iter()
            .any(|event| matches!(event, ThreadEvent::TurnFailed(_))),
        "JSONL must emit TurnFailed: {:?}",
        collected.events
    );
    assert!(
        collected
            .events
            .iter()
            .all(|event| !matches!(event, ThreadEvent::TurnCompleted(_))),
        "JSONL must not emit TurnCompleted for a reclassified empty result: {:?}",
        collected.events
    );
    let failed = collected
        .events
        .iter()
        .find(|event| matches!(event, ThreadEvent::TurnFailed(_)))
        .expect("TurnFailed present");
    let json = serde_json::to_value(failed).expect("serialize thread event");
    assert_eq!(json["type"], "turn.failed");
}

/// LIM-134 F4: the human processor suppresses the last-message file and
/// takes the Failed arm (which reports the error) rather than completing.
#[tokio::test]
async fn reclassified_empty_result_human_suppresses_final_message() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config");
    let output_dir = tempdir().expect("create temp output dir");
    let last_message_path = output_dir.path().join("last-message.txt");
    std::fs::write(&last_message_path, "keep existing contents").expect("seed last message");

    let mut processor = EventProcessorWithHumanOutput::create_with_ansi(
        /*with_ansi*/ false,
        &config,
        Some(last_message_path.clone()),
    );
    let evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
    let mut notification = completed_turn(Vec::new());
    evidence.reclassify_empty_result(&mut notification);

    let status = crate::event_processor::EventProcessor::process_server_notification(
        &mut processor,
        notification,
    );
    assert_eq!(status, CodexStatus::InitiateShutdown);
    crate::event_processor::EventProcessor::print_final_output(&mut processor);
    assert_eq!(
        std::fs::read_to_string(&last_message_path).expect("read last message"),
        "keep existing contents",
        "Failed completion must not write a fabricated/stale final message"
    );
}

/// LIM-134 F4(c): the error_seen decision that leads to `std::process::exit(1)`.
///
/// Limitation: the `exit(1)` call itself is not executed here — it lives in
/// the process event loop and would terminate the test process. This asserts
/// the production decision boundary used immediately before that exit.
#[test]
fn reclassified_empty_result_sets_the_nonzero_exit_decision() {
    let evidence = DirectTurnAnswerEvidence::new(/*expected*/ true);
    let mut notification = completed_turn(Vec::new());
    evidence.reclassify_empty_result(&mut notification);
    assert!(
        notification_sets_error_seen(&notification, "thread-1", "turn-1"),
        "a reclassified Failed turn must set error_seen for this thread/turn"
    );
    assert!(
        !notification_sets_error_seen(&notification, "other-thread", "turn-1"),
        "error_seen is scoped to the primary thread"
    );

    let success = completed_turn(vec![agent_message("msg-1", "the answer")]);
    assert!(
        !notification_sets_error_seen(&success, "thread-1", "turn-1"),
        "a successful completion must not set error_seen"
    );
}
