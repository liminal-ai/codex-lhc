//! CX-S6 (LIM-106) certification canaries, updated to Story 5 MidTurn law.
//!
//! Each canary drives a production seam with one real fault. A clean thread's
//! ordinary MidTurn arm is turn-parts (`try_run_lhc_compact_arm`); protected
//! escalation, live-pair graft, host-body degrade, and validation ACK live on
//! the typed compact-continuation runtime (`run_mid_turn_forced_boundary_continuation`)
//! — the same split the mid-turn suite uses. Never native compact.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use codex_analytics::CompactionPhase;
use codex_lhc_host::DerivedProvenance;
use codex_lhc_host::LhcCaptureSlot;
use codex_lhc_host::TRUNCATION_MARKER;
use codex_lhc_host::content_identity_digest;
use codex_lhc_host::host_items_missing_from_archive_with_provenance;
use codex_lhc_host::wait_for_handle;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serial_test::serial;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::LhcCompactAttempt;
use super::mid_turn_tests::assert_provider_sendable;
use super::mid_turn_tests::decision_epoch;
use super::mid_turn_tests::inject_response_usage;
use super::mid_turn_tests::install_lhc_midturn;
use super::mid_turn_tests::mid_facts;
use super::mid_turn_tests::sample_usage;
use super::mid_turn_tests::seed_turns;
use super::run_mid_turn_forced_boundary_continuation;
use super::try_run_lhc_compact_arm;
use crate::compact::InitialContextInjection;
use crate::session::tests::make_session_and_context;

/// Bound every canary shares: the seam is allowed to degrade, never to hang.
const CANARY_BOUND: Duration = Duration::from_secs(60);

/// Host safe-runway the oversized canary's assembled body cannot fit under
/// without the degrade ladder cutting model-visible content.
const OVERSIZED_CANARY_RUNWAY_TOKENS: i64 = 1_500;

fn custom_call(call_id: &str, status: Option<&str>) -> ResponseItem {
    ResponseItem::CustomToolCall {
        id: None,
        status: status.map(str::to_string),
        call_id: call_id.into(),
        name: "exec".into(),
        namespace: None,
        input: "{\"cmd\":\"canary\"}".into(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn custom_output(call_id: &str, body: &str) -> ResponseItem {
    ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.into(),
        name: Some("exec".into()),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(body.into()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

fn protected_items(body: &[ResponseItem], call_id: &str) -> (usize, usize) {
    let calls = body
        .iter()
        .filter(|i| codex_lhc_host::client_call_id(i).as_deref() == Some(call_id))
        .count();
    let outputs = body
        .iter()
        .filter(|i| codex_lhc_host::output_call_id(i).as_deref() == Some(call_id))
        .count();
    (calls, outputs)
}

/// Canary (a) — Story 5 truthful seam (AC-7.4).
///
/// Derivations reported missing/failed *and* a capture worker that never
/// acknowledges the arm's flush. An incomplete flush is not a settled seam:
/// the ordinary MidTurn arm keeps the current body, allows the next seam,
/// never asserts `captureFlushed`, and never falls open to native compact.
/// Maps to `mid_turn_blocked_capture_flush_is_not_a_settled_seam`.
#[tokio::test]
#[serial]
async fn canary_degraded_capture_plus_flush_timeout_still_yields_a_body() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    // Fault 1 (G6/S4): derivation material is degraded.
    slot.set_mid_turn_test_hooks(Some(codex_lhc_host::MidTurnTestHooks {
        force_derivations_missing_or_failed: Some(true),
        ..Default::default()
    }));
    let handle = wait_for_handle(&slot, CANARY_BOUND).await.expect("handle");
    seed_turns(&session, &tc, 16).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    // Fault 2 (G9/G10): the worker is parked, so the arm's flush can never be
    // acknowledged and queued capture work sits behind the park.
    let release = handle.block_worker().await;
    handle.persist(
        &ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "queued behind the parked capture worker".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        codex_extension_api::RawItemProvenance::UserPrompt,
        /*step_index*/ None,
    );

    let sess = Arc::new(session);
    let history_before = sess.clone_history().await.raw_items().count();
    let started = std::time::Instant::now();
    let attempt = tokio::time::timeout(
        CANARY_BOUND,
        try_run_lhc_compact_arm(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            /*manual*/ false,
            CompactionPhase::MidTurn,
            Some(mid_facts(
                "canary-degraded-capture-and-flush",
                true,
                decision_epoch(&sess),
                Vec::new(),
                Some(sample_usage(5_000)),
            )),
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("degraded capture behind a parked worker must not hang the seam")
    .expect("arm");
    let elapsed = started.elapsed();
    drop(release);
    assert!(
        elapsed < CANARY_BOUND,
        "the flush bound keeps the seam bounded; took {elapsed:?}"
    );

    // Story 5: a flush that does not complete is not a settled seam. The
    // ordinary arm must not install, must not strand the turn, and must not
    // fall open to native compact.
    match &attempt {
        LhcCompactAttempt::MidTurnBlocked {
            reason,
            next_provider_request_allowed,
        } => {
            assert!(
                reason.contains("flush") && reason.contains("not a settled seam"),
                "blocked on the flush fact: {reason}"
            );
            assert!(
                *next_provider_request_allowed,
                "an unsettled seam retries later; it never strands the turn"
            );
        }
        other => panic!(
            "incomplete capture flush must keep the current body and retry later, got {other:?}"
        ),
    }
    let history_after = sess.clone_history().await.raw_items().count();
    assert_eq!(
        history_after, history_before,
        "unsettled flush preserves the current body: before={history_before} after={history_after}"
    );
    assert!(
        !matches!(attempt, LhcCompactAttempt::Unavailable { .. }),
        "never a license for native compact: {attempt:?}"
    );
}

/// Canary (b) — R1/R7. Input that arrives *while the body is being
/// constructed* is not a veto. The steer belongs to the next turn: the compact
/// installs and the queued input is still there to drain.
#[tokio::test]
#[serial]
async fn canary_input_arriving_during_construction_does_not_suppress_install() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, CANARY_BOUND).await.expect("handle");
    seed_turns(&session, &tc, 16).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let history_before = sess.clone_history().await.raw_items().count();
    assert!(
        !sess.input_queue.has_pending_mailbox_items().await,
        "no queued input before the seam"
    );

    // Real concurrency: the mailbox enqueue races the arm's construction rather
    // than being staged before the decision.
    let steering = {
        let sess = Arc::clone(&sess);
        tokio::spawn(async move {
            for i in 0..8 {
                sess.input_queue
                    .enqueue_mailbox_communication(
                        codex_protocol::protocol::InterAgentCommunication::new(
                            codex_protocol::AgentPath::root(),
                            codex_protocol::AgentPath::try_from("/root/worker").expect("path"),
                            Vec::new(),
                            format!("steer during construction {i}"),
                            /*trigger_turn*/ false,
                        ),
                        /*parent_turn_id*/ None,
                        /*root_turn_id*/ None,
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
    };

    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "canary-input-during-construction",
            true,
            epoch,
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    steering.await.expect("steering task");

    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("input arriving during construction must not suppress compact, got {attempt:?}");
    };
    assert!(!body.is_empty(), "installed body must be non-empty");
    assert_provider_sendable(body);
    let history_after = sess.clone_history().await.raw_items().count();
    assert!(
        history_after < history_before,
        "the session must end up smaller: before={history_before} after={history_after}"
    );
    assert_ne!(
        epoch,
        decision_epoch(&sess),
        "the canary is only meaningful if input really did arrive during the seam"
    );
    assert!(
        sess.input_queue.has_pending_mailbox_items().await,
        "the steer belongs to the next turn and must survive the compact"
    );
}

/// Canary (d) — R8. The exact provider-native protected pair cannot be proven
/// (ambiguous live cardinality), so the body carries the LHC-reconstructed
/// pair instead: same call_id, same correlation, provider-specific fields
/// missing. Degraded body, valid request, install proceeds.
///
/// Graft runs on the typed compact-continuation runtime (host_validation
/// spec), the same path `mid_turn_protected_escalation_validates_installs_and_clears_reload_gate`
/// pins. A parked worker would make the ordinary parts flush unsettled and
/// cannot be used to prove a live-only graft.
#[tokio::test]
#[serial]
async fn canary_unprovable_graft_installs_the_lhc_pair_not_the_exact_pair() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    slot.set_mid_turn_test_safe_runway(Some(5_000));
    slot.set_mid_turn_test_compact(Some(codex_lhc_host::test_compact_opts(400.0)));
    let handle = wait_for_handle(&slot, CANARY_BOUND).await.expect("handle");
    seed_turns(&session, &tc, 2).await;

    // A provider-native custom-tool pair: `status` and the output `name` are
    // exactly the fields the LHC reconstruction cannot carry.
    let protected_id = "canary-graft-call";
    let live_call = custom_call(protected_id, Some("completed"));
    session
        .record_conversation_items_with_provenance(
            &tc,
            &[
                live_call.clone(),
                custom_output(protected_id, &format!("{}-CANARY", "tok ".repeat(400))),
            ],
            codex_extension_api::RawItemProvenance::ModelOutput,
        )
        .await;
    inject_response_usage(&session, &tc, 4_800).await;
    handle.flush().await;

    // Live-side ambiguity after a truthful flush: a second call for the same
    // id is injected into host history only (not captured). The worker is
    // not parked, so the arm's flush still settles. SDK correlation stays
    // proven from the captured pair; `graft_live_protected_pairs` sees two
    // live calls and cannot prove the exact pair.
    let mut live: Vec<_> = session.clone_history().await.raw_items().cloned().collect();
    live.push(custom_call(protected_id, Some("completed")));
    session.replace_history(live, None).await;

    let sess = Arc::new(session);
    let attempt = run_mid_turn_forced_boundary_continuation(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        mid_facts(
            "canary-degraded-graft",
            true,
            decision_epoch(&sess),
            vec![protected_id.into()],
            Some(sample_usage(4_800)),
        ),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");

    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("an unprovable graft must degrade and install, got {attempt:?}");
    };
    assert_provider_sendable(body);
    let (calls, outputs) = protected_items(body, protected_id);
    assert_eq!(
        (calls, outputs),
        (1, 1),
        "the degraded body still carries exactly one correlated protected pair"
    );
    let installed_call = body
        .iter()
        .find(|i| codex_lhc_host::client_call_id(i).as_deref() == Some(protected_id))
        .expect("protected call in body");
    assert_ne!(
        codex_lhc_host::item_bytes_without_id(installed_call),
        codex_lhc_host::item_bytes_without_id(&live_call),
        "this canary is only meaningful if the exact live pair was NOT grafted"
    );
}

/// Canary (e) — R9. Stable ids are preferred identity, not the only identity.
/// With every id stripped from a real installed body, content digests alone
/// still account for the whole body against the canonical archive — so nothing
/// re-imports and resume equivalence holds without ids.
#[tokio::test]
#[serial]
async fn canary_installed_body_identity_survives_on_digests_without_stable_ids() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, CANARY_BOUND).await.expect("handle");
    seed_turns(&session, &tc, 16).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;
    let thread_id = handle.thread_id().to_string();
    let thread_root = handle.root().map(std::path::Path::to_path_buf);

    let sess = Arc::new(session);
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "canary-digest-identity",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");
    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("expected Installed: {attempt:?}");
    };

    // The production identity set: digests are computed unconditionally for
    // every installed item (`compact_lhc::install_lhc_compact_rewrite`).
    let digests: HashSet<String> = body.iter().map(content_identity_digest).collect();
    assert_eq!(
        digests.len(),
        body.iter()
            .map(content_identity_digest)
            .collect::<Vec<_>>()
            .len(),
        "digest identity must be assignable for every installed item"
    );

    // The R9 world: nothing in the body carries an assignable stable id.
    let anonymous: Vec<ResponseItem> = body
        .iter()
        .cloned()
        .map(|mut item| {
            item.set_id(None);
            item
        })
        .collect();
    assert!(
        anonymous
            .iter()
            .all(|i| codex_lhc_host::item_stable_id(i).is_none()),
        "the canary must actually run with no stable ids"
    );

    let events = canary_archive_events(&thread_id, thread_root.as_deref()).await;
    let digest_only = DerivedProvenance {
        ids: HashSet::new(),
        digests: digests.clone(),
    };
    assert_eq!(
        host_items_missing_from_archive_with_provenance(&anonymous, &events, &digest_only),
        Vec::<ResponseItem>::new(),
        "digest identity alone must account for the whole installed body"
    );

    // Control: without the digests the same id-less body is *not* accounted
    // for, so the assertion above is carried by digest identity and not by a
    // vacuous coverage rule.
    assert!(
        !host_items_missing_from_archive_with_provenance(
            &anonymous,
            &events,
            &DerivedProvenance::default()
        )
        .is_empty(),
        "without digests the id-less body has no identity to match on"
    );
}

async fn canary_archive_events(
    thread_id: &str,
    root: Option<&std::path::Path>,
) -> Vec<codex_lhc_host::EventRecord> {
    let callbacks =
        codex_lhc_host::lhc_inference_callbacks(false).expect("deterministic offline callbacks");
    let (session, _) =
        codex_lhc_host::LhcSession::open_with_inference(thread_id, None, root, callbacks)
            .await
            .expect("open archive");
    let events = session.list_events().await.expect("list events");
    session.close().await;
    events
}

/// Canary (f) — R5/R23-S8. A durable writer claim owned by a *dead* attempt is
/// stale state, not a live competitor: the host ownership registry reports no
/// live owner for the thread, so the claim is reclaimed and compact proceeds.
#[tokio::test]
#[serial]
async fn canary_dead_owner_writer_claim_is_reclaimed_and_compact_proceeds() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    let handle = wait_for_handle(&slot, CANARY_BOUND).await.expect("handle");
    seed_turns(&session, &tc, 16).await;
    inject_response_usage(&session, &tc, 5_000).await;
    handle.flush().await;
    let thread_id = handle.thread_id().to_string();
    let thread_root = handle.root().map(std::path::Path::to_path_buf);

    // The dead owner: a prior process left the durable writer row behind and
    // holds nothing in this process's ownership registry.
    codex_lhc_host::seed_mid_turn_writer_claim_for_tests(
        &thread_id,
        thread_root.as_deref(),
        "canary-dead-owner-attempt",
    )
    .expect("seed dead-owner writer claim");

    let sess = Arc::new(session);
    let history_before = sess.clone_history().await.raw_items().count();
    let attempt = try_run_lhc_compact_arm(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        /*manual*/ false,
        CompactionPhase::MidTurn,
        Some(mid_facts(
            "canary-reclaiming-attempt",
            true,
            decision_epoch(&sess),
            Vec::new(),
            Some(sample_usage(5_000)),
        )),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");

    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("a dead owner's stale writer claim must not block compact, got {attempt:?}");
    };
    assert!(!body.is_empty(), "installed body must be non-empty");
    assert_provider_sendable(body);
    let history_after = sess.clone_history().await.raw_items().count();
    assert!(
        history_after < history_before,
        "the reclaiming attempt must actually compact: before={history_before} after={history_after}"
    );
}

/// Canary (h) — R10/R24. Body validation reporting oversized content is a
/// truncation instruction, not a refusal: the ladder cuts model-visible text,
/// leaves a marker, and installs the smaller body.
///
/// Host-body validation is the typed compact-continuation path (LIM-67), the
/// same runtime `mid_turn_host_validation_failure_degrades_installs_and_leaves_reload_clear`
/// pins. A clean thread's parts arm does not run that ladder.
#[tokio::test]
#[serial]
async fn canary_oversized_body_truncates_and_installs_instead_of_refusing() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root.clone()).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    // A host runway the assembled body cannot fit under without cutting
    // model-visible content.
    slot.set_mid_turn_test_safe_runway(Some(OVERSIZED_CANARY_RUNWAY_TOKENS));
    slot.set_mid_turn_test_compact(Some(codex_lhc_host::test_compact_opts(400.0)));
    let handle = wait_for_handle(&slot, CANARY_BOUND).await.expect("handle");
    seed_turns(&session, &tc, 2).await;
    // The protected pair is preserved through the escalation, and its result
    // alone is far past the host runway — so the assembled body is oversized
    // for a reason no pruning ladder can remove.
    let protected_id = "canary-oversized-call";
    seed_oversized_protected_pair(&session, &tc, protected_id).await;
    inject_response_usage(&session, &tc, 4_800).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let attempt = run_mid_turn_forced_boundary_continuation(
        &sess,
        &tc,
        InitialContextInjection::DoNotInject,
        mid_facts(
            "canary-oversized-1",
            true,
            decision_epoch(&sess),
            vec![protected_id.into()],
            Some(sample_usage(4_800)),
        ),
        &CancellationToken::new(),
    )
    .await
    .expect("arm");

    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("oversized content must truncate and install, got {attempt:?}");
    };
    assert!(!body.is_empty(), "truncated body must not be empty");
    assert_provider_sendable(body);
    let serialized = serde_json::to_string(body).expect("serialize body");
    assert!(
        // The marker's leading newline is escaped by JSON; match its text.
        serialized.contains(TRUNCATION_MARKER.trim_start()),
        "the degrade ladder must truncate oversized content, not refuse the body"
    );
    assert!(
        codex_lhc_host::estimate_response_items_tokens(body) < OVERSIZED_CANARY_RUNWAY_TOKENS,
        "truncation must bring the installed body under the host runway"
    );
    // The session serves exactly what validation degraded.
    let installed: Vec<ResponseItem> = sess.clone_history().await.raw_items().cloned().collect();
    assert!(
        crate::compact_lhc::response_items_structurally_equal(&installed, body),
        "the truncated body is what the session holds"
    );

    // Receipts observe: the durable row records that this attempt's view is
    // being served, and how it degraded.
    let hv = codex_lhc_host::inspect_mid_turn_host_validation(
        &handle.thread_id().to_string(),
        handle.root(),
        "canary-oversized-1",
    )
    .await
    .expect("hv inspect")
    .expect("hv row");
    assert_eq!(hv.status, codex_lhc_host::HostValidationStatus::Ok);
    let reason = hv.reason.clone().unwrap_or_default();
    assert!(
        reason.contains("proceeded degraded") && reason.contains("safe-runway"),
        "the ok row must record that the body was oversized: {reason}"
    );
    assert!(
        reason.contains(codex_lhc_host::BodyDegradationKind::TruncatedOversizedContent.as_str()),
        "the ok row must record truncation, not a refusal: {reason}"
    );
}

/// Escalation shape whose protected tool result is, on its own, well past the
/// host safe-runway threshold the canary sets.
async fn seed_oversized_protected_pair(
    session: &crate::session::session::Session,
    tc: &crate::session::turn_context::TurnContext,
    protected_id: &str,
) {
    let mut items = Vec::new();
    for i in 0..3 {
        items.push(ResponseItem::FunctionCall {
            id: None,
            name: "shell".into(),
            namespace: None,
            arguments: format!("{{\"cmd\":\"old-{i}\"}}"),
            encrypted_function_args: None,
            call_id: format!("canary-old-{i}"),
            internal_chat_message_metadata_passthrough: None,
        });
        items.push(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(format!("canary-old-{i}")),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(format!("{}-OLD{i}", "tok ".repeat(1_200))),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        });
    }
    items.push(ResponseItem::FunctionCall {
        id: None,
        name: "shell".into(),
        namespace: None,
        arguments: "{\"cmd\":\"protected\"}".into(),
        encrypted_function_args: None,
        call_id: protected_id.into(),
        internal_chat_message_metadata_passthrough: None,
    });
    items.push(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(protected_id.into()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(format!("{}-PROTECTED", "tok ".repeat(9_000))),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    });
    session
        .record_conversation_items_with_provenance(
            tc,
            &items,
            codex_extension_api::RawItemProvenance::ModelOutput,
        )
        .await;
}

/// (g) R11 arm-level: the validation-ACK write fails and the arm warns and
/// continues — in-process AND at the next open. The protected-escalation
/// compact/install stands (no rollback, no refusal), the session serves the
/// compacted body, and the durable state stays truthful: the receipt keeps
/// its awaiting posture and no fabricated `ok` row ever appears. The missing
/// ack is a loud warning, never a veto: next-open reconciliation regenerates
/// the rollout from the installed LHC view (the prior oversized generation
/// does not stay authoritative) and converges. The ordinary repair op remains
/// available as optional bookkeeping recovery.
///
/// Host-validation ACK is the typed compact-continuation path. A clean
/// thread's parts arm never writes that receipt (`host_validation: None`).
#[tokio::test]
#[serial]
async fn canary_validation_ack_write_failure_warns_and_continues() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut session, tc) = make_session_and_context().await;
    install_lhc_midturn(&mut session, root).await;
    let slot = session
        .services
        .thread_extension_data
        .get::<LhcCaptureSlot>()
        .expect("slot");
    slot.set_mid_turn_test_upper_trigger(Some(100));
    slot.set_mid_turn_test_safe_runway(Some(5_000));
    slot.set_mid_turn_test_compact(Some(codex_lhc_host::test_compact_opts(400.0)));
    // The one fault: the R11 ACK write fails at the real write site.
    slot.set_mid_turn_test_force_validation_ack_write_fail(true);
    let handle = wait_for_handle(&slot, CANARY_BOUND).await.expect("handle");
    seed_turns(&session, &tc, 2).await;
    let protected_id = "call-prot-ackfail";
    super::mid_turn_tests::seed_escalation_history(&session, &tc, protected_id).await;
    inject_response_usage(&session, &tc, 4_800).await;
    handle.flush().await;

    let sess = Arc::new(session);
    let epoch = decision_epoch(&sess);
    let attempt = tokio::time::timeout(
        CANARY_BOUND,
        run_mid_turn_forced_boundary_continuation(
            &sess,
            &tc,
            InitialContextInjection::DoNotInject,
            mid_facts(
                "canary-ack-write-fail",
                true,
                epoch,
                vec![protected_id.into()],
                Some(sample_usage(4_800)),
            ),
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("bounded")
    .expect("arm");

    // Compact/install continues: no rollback, no refusal.
    let LhcCompactAttempt::Installed { body, .. } = &attempt else {
        panic!("ACK write failure must not stop the install, got {attempt:?}");
    };
    assert!(!body.is_empty(), "installed body must not be empty");
    assert_provider_sendable(body);
    let installed: Vec<_> = sess.clone_history().await.raw_items().cloned().collect();
    assert!(
        crate::compact_lhc::response_items_structurally_equal(&installed, body),
        "the session serves the compacted body despite the failed ACK write"
    );

    let thread_id = handle.thread_id().to_string();
    let root_path = handle.root().map(std::path::Path::to_path_buf);

    // Core install retained: boundary + marker survive the failed ACK write.
    let receipts =
        codex_lhc_host::inspect_compact_continuation_receipts(&thread_id, root_path.as_deref())
            .await
            .expect("receipts");
    let last = receipts.last().expect("receipt");
    let cont = last
        .continuation_turn_id
        .as_deref()
        .expect("continuation turn id");
    assert!(
        codex_lhc_host::inspect_has_compact_continuation_marker(
            &thread_id,
            root_path.as_deref(),
            cont
        )
        .await
        .expect("marker"),
        "core install (boundary + marker) is retained through the ACK-write failure"
    );

    // Truthful state, no partial/fabricated row: the durable HV row was never
    // written ok. It is either absent or still awaiting — never Ok.
    let hv = codex_lhc_host::inspect_mid_turn_host_validation(
        &thread_id,
        root_path.as_deref(),
        "canary-ack-write-fail",
    )
    .await
    .expect("hv inspect");
    assert!(
        !matches!(
            hv.as_ref().map(|row| row.status),
            Some(codex_lhc_host::HostValidationStatus::Ok)
        ),
        "a failed ACK write must never leave a fabricated ok row: {hv:?}"
    );

    // R11: the unresolved ack is a WARNING descriptor, never a veto.
    assert!(
        codex_lhc_host::host_validation_reload_warning(&thread_id, root_path.as_deref())
            .await
            .is_some(),
        "the unresolved ack must be loudly describable for the warning"
    );

    // NEXT OPEN (G25 under R11): a stale prior-generation rollout must NOT
    // stay authoritative because the ack is missing. Reconciliation proceeds
    // from the installed LHC view, produces a provider-sendable compacted
    // generation, and converges.
    let rollout_dir = tempdir().unwrap();
    let rollout_path = rollout_dir.path().join("canary-ack-fail.jsonl");
    codex_lhc_host::atomic_rewrite_rollout(
        &rollout_path,
        &[codex_history::RolloutItem::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![ContentItem::InputText {
                    text: "stale prior-generation rollout".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        )],
    )
    .expect("seed stale rollout");
    let reconciled = codex_lhc_host::reconcile_rollout_at_path(
        &rollout_path,
        &thread_id,
        root_path.as_deref(),
        None,
    )
    .await;
    let codex_lhc_host::ReconcileOutcome::Regenerated { items, .. } = reconciled else {
        panic!(
            "next open must regenerate from the installed view, not preserve the \
             prior generation: {reconciled:?}"
        );
    };
    assert!(
        items > 0,
        "regenerated rollout must carry the compacted body"
    );
    // Convergence: a second open finds the regenerated file authoritative.
    let second = codex_lhc_host::reconcile_rollout_at_path(
        &rollout_path,
        &thread_id,
        root_path.as_deref(),
        None,
    )
    .await;
    assert_eq!(
        second,
        codex_lhc_host::ReconcileOutcome::Unchanged { reason: "ok" },
        "second open must converge with no repeated rewrite"
    );
    // Still no fabricated ok row after the whole next-open path.
    let hv_after = codex_lhc_host::inspect_mid_turn_host_validation(
        &thread_id,
        root_path.as_deref(),
        "canary-ack-write-fail",
    )
    .await
    .expect("hv inspect");
    assert!(
        !matches!(
            hv_after.as_ref().map(|row| row.status),
            Some(codex_lhc_host::HostValidationStatus::Ok)
        ),
        "next-open regeneration must not fabricate an ok row: {hv_after:?}"
    );

    // Optional bookkeeping recovery (not required for service): once the
    // write path works again, the ordinary repair op records the ack and the
    // warning clears.
    slot.set_mid_turn_test_force_validation_ack_write_fail(false);
    codex_lhc_host::record_mid_turn_host_validation(
        &thread_id,
        root_path.as_deref(),
        "canary-ack-write-fail",
        /*ok*/ true,
        Some("ack retried after transient write failure".into()),
    )
    .await
    .expect("ack retry");
    assert!(
        codex_lhc_host::host_validation_reload_warning(&thread_id, root_path.as_deref())
            .await
            .is_none(),
        "a recorded ack clears the warning"
    );
}
