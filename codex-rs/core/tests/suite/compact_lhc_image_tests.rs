//! Image-bearing native tool output through a real MidTurn provider loop.
use super::*;
use base64::Engine;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_image_payload_survives_mid_turn_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let root = TempDir::new()?;
    let image_path = root.path().join("fixture.png");
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    std::fs::write(
        &image_path,
        base64::engine::general_purpose::STANDARD.decode(PNG)?,
    )?;
    let mut done = ev_completed_with_tokens("image-tool-1", 60_000);
    done["response"]["end_turn"] = json!(false);
    let mut warmup_done = ev_completed_with_tokens("image-warmup", 60_000);
    warmup_done["response"]["end_turn"] = json!(false);
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("image-warmup"),
                ev_assistant_message(
                    "image-warmup-text",
                    &"completed preparatory analysis ".repeat(40_000),
                ),
                warmup_done,
            ]),
            sse(vec![
                ev_response_created("image-tool-1"),
                ev_function_call(
                    "image-call",
                    "view_image",
                    &json!({"path":image_path,"detail":"original"}).to_string(),
                ),
                done,
            ]),
            sse(vec![
                ev_response_created("image-tool-2"),
                ev_assistant_message("image-answer", "image inspected"),
                ev_completed_with_tokens("image-tool-2", 80),
            ]),
        ],
    )
    .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.5", |model| {
            model.context_window = Some(400_000);
            model.max_context_window = Some(400_000);
            model.auto_compact_token_limit = Some(10_000);
        })
        .with_extensions(lhc_extensions(root.path().join("lhc")))
        .with_config(move |config| {
            config.model_provider = provider;
            let _ = config.features.enable(Feature::LhcCapture);
            let _ = config.features.disable(Feature::TokenBudget);
            config.model_auto_compact_token_limit = Some(10_000);
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            config.model_context_window = Some(400_000);
        });
    let test = builder.build(&server).await?;
    arm_midturn_knobs(&test.codex).await;
    test.codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot")
        .set_mid_turn_test_compact(Some(test_compact_opts(120_000.0)));
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "inspect the image".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    ..Default::default()
                },
            ),
        )
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    let slot = test
        .codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("capture");
    let view = codex_lhc_host::inspect_installed_view(handle.thread_id(), handle.root())
        .await
        .expect("installed view")
        .expect("a compact must install a view");
    assert!(
        codex_lhc_host::view_serves_parts(&view),
        "image test must cross an actual turn-parts compact: {view:?}"
    );
    let bodies = request_bodies(&mock);
    assert_eq!(bodies.len(), 3);
    let request: serde_json::Value = serde_json::from_str(&bodies[2])?;
    let output = request["input"]
        .as_array()
        .expect("input")
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "image-call")
        .expect("paired view_image output after compaction");
    assert!(
        output["output"].as_array().is_some_and(|parts| parts
            .iter()
            .any(|part| part["type"] == "input_image"
                && part["image_url"] == format!("data:image/png;base64,{PNG}"))),
        "post-compact provider request must carry actual tool image bytes: {output}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pasted_image_uses_placeholder_when_its_part_is_compressed_across_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let lhc_root = TempDir::new()?;
    let root = lhc_root.path().to_path_buf();

    let mut continue_done = ev_completed_with_tokens("resp-1", /*total_tokens*/ 60_000);
    continue_done["response"]["end_turn"] = json!(false);

    let first = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("m1", &"working on long task part one ".repeat(40_000)),
        continue_done,
    ]);
    let second = sse(vec![
        ev_response_created("resp-2"),
        ev_assistant_message("m2", "task complete after mid-turn compact"),
        ev_completed_with_tokens("resp-2", /*total_tokens*/ 80),
    ]);
    let mut middle_done = ev_completed_with_tokens("resp-middle", 60_000);
    middle_done["response"]["end_turn"] = json!(false);
    let middle = sse(vec![
        ev_response_created("resp-middle"),
        ev_assistant_message("m-middle", "continue with retained image"),
        middle_done,
    ]);
    let mock = mount_sse_sequence(&server, vec![first, middle, second]).await;

    let model_provider = non_openai_model_provider(&server);
    let extensions = lhc_extensions(root);
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.5", |model| {
            model.context_window = Some(400_000);
            model.max_context_window = Some(400_000);
            model.auto_compact_token_limit = Some(10_000);
        })
        .with_extensions(extensions)
        .with_config(move |config| {
            config.model_provider = model_provider;
            let _ = config.features.enable(Feature::LhcCapture);
            let _ = config.features.disable(Feature::TokenBudget);
            config.model_auto_compact_token_limit = Some(10_000);
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            config.model_context_window = Some(400_000);
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.into());
        });
    let test = builder.build(&server).await?;
    arm_midturn_knobs(&test.codex).await;
    test.codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot")
        .set_mid_turn_test_compact(Some(test_compact_opts(120_000.0)));

    let image_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![
            UserInput::Image {
                image_url: image_url.into(),
                detail: Some(codex_protocol::models::ImageDetail::Original),
            },
            UserInput::Text {
                text: "continue this long agentic task".into(),
                text_elements: Vec::new(),
            },
        ]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let bodies = request_bodies(&mock);
    assert_eq!(bodies.len(), 3, "two continuations followed by completion");

    let req2 = &bodies[2];
    let request: serde_json::Value = serde_json::from_str(req2)?;
    assert!(
        request["input"]
            .as_array()
            .expect("input")
            .iter()
            .any(|item| {
                item["content"].as_array().is_some_and(|content| {
                    content.iter().any(|part| {
                        part["type"] == "input_text"
                            && part["text"]
                                .as_str()
                                .is_some_and(|text| text.contains("[image · image/png · 70 B]"))
                    })
                })
            }),
        "compressed image must be a bounded placeholder"
    );
    assert!(
        !req2.contains(image_url),
        "compressed bands must not embed image bytes in text"
    );
    let slot = test
        .codex
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("slot");
    let handle = wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("handle");
    let view = codex_lhc_host::inspect_installed_view(handle.thread_id(), handle.root())
        .await
        .expect("installed view")
        .expect("a compact must install a view");
    assert!(
        codex_lhc_host::view_serves_parts(&view),
        "image test must cross an actual turn-parts compact: {view:?}"
    );
    let resumed_response = sse(vec![
        ev_response_created("resp-resume"),
        ev_assistant_message("m-resume", "resumed"),
        ev_completed_with_tokens("resp-resume", 80),
    ]);
    // Background LHC derivation shares this provider; keep it from consuming
    // the mock reserved for the agent's first resumed request.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(resumed_response.clone()),
        )
        .with_priority(10)
        .mount(&server)
        .await;
    let resumed_mock = core_test_support::responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            String::from_utf8_lossy(&request.body).contains("continue after restart")
        },
        resumed_response,
    )
    .await;
    let resumed = builder.restart(&server, &test).await?;
    resumed
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "continue after restart".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&resumed.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    let resumed_bodies = request_bodies(&resumed_mock);
    let resumed_body = resumed_bodies
        .iter()
        .find(|body| body.contains("continue after restart"))
        .expect("agent request after restart, separate from background LHC derivation requests");
    let resumed_request: serde_json::Value = serde_json::from_str(resumed_body)?;
    assert!(
        resumed_request["input"]
            .as_array()
            .expect("input")
            .iter()
            .any(|item| {
                item["content"].as_array().is_some_and(|content| {
                    content.iter().any(|part| {
                        part["type"] == "input_text"
                            && part["text"]
                                .as_str()
                                .is_some_and(|text| text.contains("[image · image/png · 70 B]"))
                    })
                })
            }),
        "compressed image placeholder must survive resume"
    );
    assert!(!resumed_body.contains(image_url));
    Ok(())
}
