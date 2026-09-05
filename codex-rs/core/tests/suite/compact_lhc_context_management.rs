//! The LHC product rejects native notes/reset semantics before model exposure.

use std::sync::Arc;

use anyhow::Result;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use tempfile::TempDir;

#[tokio::test]
async fn rejects_notes_reset_before_provider_requests_at_any_threshold() -> Result<()> {
    skip_if_no_network!(Ok(()));
    // A deliberately unavailable notes backend must not weaken rejection.
    // Both explicit token-budget mode and experimental auto-activation are gated.
    for feature in [Feature::TokenBudget, Feature::ContextManagement] {
        for threshold in [1, 1_000_000] {
            let server = start_mock_server().await;
            let root = TempDir::new()?;
            let mut registry = ExtensionRegistryBuilder::<codex_core::config::Config>::new();
            codex_lhc_host::install_with_root(
                &mut registry,
                |config| config.features.enabled(Feature::LhcCapture),
                root.path().to_path_buf(),
            );
            let result = test_codex()
                .with_extensions(Arc::new(registry.build()))
                .with_config(move |config| {
                    let _ = config.features.enable(Feature::LhcCapture);
                    let _ = config.features.enable(feature);
                    config.model_auto_compact_token_limit = Some(threshold);
                })
                .build(&server)
                .await;
            let error = result.err().expect("incompatible mode must fail startup");
            assert!(
                format!("{error:#}").contains("LHC does not support the upstream notes/reset mode")
            );
            let requests = server.received_requests().await.expect("requests");
            assert!(
                !requests
                    .iter()
                    .any(|request| request.url.path().contains("/responses")
                        || request.url.path().contains("/notes/")),
                "rejection must precede model context, tools, and notes calls"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn default_lhc_mode_exposes_no_native_reset_tool() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let root = TempDir::new()?;
    let mut registry = ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    codex_lhc_host::install_with_root(
        &mut registry,
        |config| config.features.enabled(Feature::LhcCapture),
        root.path().to_path_buf(),
    );
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("default-lhc"),
            ev_assistant_message("default-answer", "done"),
            ev_completed("default-lhc"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_extensions(Arc::new(registry.build()))
        .with_config(|config| {
            let _ = config.features.enable(Feature::LhcCapture);
            let _ = config.features.disable(Feature::TokenBudget);
            let _ = config.features.disable(Feature::ContextManagement);
        });
    let test = builder.build(&server).await?;
    test.submit_turn("continue with LHC").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let body = requests[0].body_json();
    assert!(
        !body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["name"] == "new_context")
    );
    test.codex.shutdown_and_wait().await?;
    let rollout = test
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout");
    let before = std::fs::read(&rollout)?;
    builder = builder.with_config(|config| {
        let _ = config.features.enable(Feature::TokenBudget);
    });
    let error = builder
        .resume(&server, Arc::clone(&test.home), rollout.clone())
        .await
        .err()
        .expect("incompatible resume must fail before a provider request");
    assert!(format!("{error:#}").contains("LHC does not support the upstream notes/reset mode"));
    assert_eq!(
        std::fs::read(rollout)?,
        before,
        "rejected resume must preserve recorded history"
    );
    assert_eq!(mock.requests().len(), 1);
    Ok(())
}
