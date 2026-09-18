//! Compaction checkpoints retain the summary captured after a mid-turn settings update.

use super::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_history::RolloutItem;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::install_with_root;
use codex_lhc_host::wait_for_handle;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use test_case::test_case;

fn lhc_extensions(root: PathBuf) -> Arc<codex_extension_api::ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    install_with_root(
        &mut builder,
        |config| config.features.enabled(Feature::LhcCapture),
        root,
    );
    Arc::new(builder.build())
}

async fn wait_lhc_capture(thread: &CodexThread) {
    let slot = thread
        .thread_extension_data()
        .get::<LhcCaptureSlot>()
        .expect("LhcCaptureSlot");
    wait_for_handle(&slot, Duration::from_secs(30))
        .await
        .expect("LHC capture ready");
}

#[derive(Clone, Copy)]
enum CompactionMode {
    Local,
    RemoteV2,
}

#[test_case(CompactionMode::Local; "local")]
#[test_case(CompactionMode::RemoteV2; "remote v2")]
#[tokio::test]
async fn compaction_preserves_updated_summary(mode: CompactionMode) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let bodies = vec![
        paused_response("pause", "pause-call"),
        sse(vec![
            ev_function_call(
                "plan",
                "update_plan",
                &json!({"plan": [{"step": "Finish the answer", "status": "in_progress"}]})
                    .to_string(),
            ),
            responses::ev_completed_with_tokens("plan-response", /*total_tokens*/ 330_000),
        ]),
        sse_completed("done"),
    ];
    let responses = mount_sse_sequence(&server, bodies).await;
    let lhc_root = TempDir::new()?;
    let test = step_settings_test()
        .with_extensions(lhc_extensions(lhc_root.path().to_path_buf()))
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            config.model_auto_compact_token_limit = Some(200_000);
            config
                .features
                .enable(Feature::LhcCapture)
                .expect("enable LHC capture");
            config
                .features
                .disable(Feature::TokenBudget)
                .expect("disable token budget");
            let _ = config.features.disable(Feature::ContextManagement);
            config.model_provider.name = match mode {
                CompactionMode::Local => "Local compaction test",
                CompactionMode::RemoteV2 => "OpenAI",
            }
            .to_string();
        })
        .build_with_auto_env(&server)
        .await?;
    wait_lhc_capture(&test.codex).await;
    let pause = start_paused_turn(&test.codex).await?;
    assert_eq!(
        submit_turn_settings(
            &test.codex,
            &pause.turn_id,
            TurnSettingsUpdate {
                summary: Some(ReasoningSummary::Detailed),
                ..Default::default()
            }
        )
        .await?,
        TurnSettingsUpdateOutcome::Applied
    );
    answer_paused_turn(&test.codex, &pause.turn_id).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.codex.shutdown_and_wait().await?;

    let requests = responses.requests();
    assert_eq!(
        [
            requests[0].body_json()["reasoning"]["summary"].clone(),
            requests[1].body_json()["reasoning"]["summary"].clone()
        ],
        [json!("concise"), json!("detailed")]
    );
    let rollout =
        std::fs::read_to_string(test.session_configured.rollout_path.expect("rollout path"))?;
    let items = rollout
        .lines()
        .map(codex_rollout::parse_rollout_line)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let checkpoint = items
        .iter()
        .skip_while(|line| !matches!(line.item, RolloutItem::Compacted(_)))
        .skip(/*n*/ 1)
        .take_while(|line| !matches!(line.item, RolloutItem::EventMsg(_)))
        .find_map(|line| match &line.item {
            RolloutItem::TurnContext(context) => Some(context.summary),
            _ => None,
        });
    assert_eq!(checkpoint, Some(ReasoningSummary::Detailed));
    Ok(())
}
