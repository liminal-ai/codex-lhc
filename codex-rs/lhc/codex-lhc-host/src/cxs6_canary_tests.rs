//! CX-S6 (LIM-106) host-side certification canary.
//!
//! R11: the durable host-validation acknowledgment is a receipt. This canary
//! holds the receipt's write to the same standard the ruling sets — it is a
//! separable, fallible durable write, and when it fails it records nothing and
//! gates nothing. The core-side consequence (the body is installed anyway) is
//! the warn-only call site in `codex-core`'s `install_lhc_compact_rewrite`,
//! covered end-to-end by `compact_lhc_mid_turn_tests::
//! mid_turn_host_validation_failure_degrades_installs_and_leaves_reload_clear`.

use pretty_assertions::assert_eq;
use tempfile::tempdir;

use crate::HostValidationStatus;
use crate::LhcSession;
use crate::host_validation_reload_block;
use crate::inspect_mid_turn_host_validation;
use crate::lhc_inference_callbacks;
use crate::record_mid_turn_host_validation;

/// R11 (CX-S3): a validation ACK that cannot be written leaves no durable
/// trace and raises no gate. The thread it failed against stays fully intact
/// and readable, so nothing about the failed write can reach back and decide
/// whether the session compacts. Receipts observe, never govern.
#[tokio::test]
async fn canary_validation_ack_write_failure_records_nothing_and_gates_nothing() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let thread_id = "cxs6-ack-canary";

    let callbacks = lhc_inference_callbacks(false).expect("deterministic offline callbacks");
    let (session, _) = LhcSession::open_with_inference(thread_id, None, Some(&root), callbacks)
        .await
        .expect("open canary thread");
    session.close().await;

    // Control: on a live thread the receipt lands, so the failures below are
    // failures of the write and not of the fixture.
    let ack = record_mid_turn_host_validation(
        thread_id,
        Some(&root),
        "cxs6-ack-control",
        /*ok*/ true,
        Some("control receipt".into()),
    )
    .await
    .expect("control ack must be writable");
    assert_eq!(ack.status, HostValidationStatus::Ok);

    // Failure mode 1 — the storage the receipt targets is unreachable.
    let unreachable_root = dir.path().join("no-such-lhc-root");
    let err = record_mid_turn_host_validation(
        thread_id,
        Some(&unreachable_root),
        "cxs6-ack-unwritable",
        /*ok*/ true,
        Some("proceeded degraded: canary".into()),
    )
    .await
    .expect_err("ack write against unreachable storage must fail");
    assert!(
        err.contains("thread file missing"),
        "the failure must name the unwritable receipt, not a body problem: {err}"
    );

    // Failure mode 2 — the certified writer refuses the record itself.
    let err = record_mid_turn_host_validation(
        thread_id,
        Some(&root),
        /*attempt_id*/ "",
        /*ok*/ true,
        Some("proceeded degraded: canary".into()),
    )
    .await
    .expect_err("ack write with no attempt identity must fail");
    assert!(
        err.contains("record host validation"),
        "the failure must come from the receipt writer: {err}"
    );

    // Nothing was recorded for the attempt whose receipt could not be written.
    assert_eq!(
        inspect_mid_turn_host_validation(thread_id, Some(&root), "cxs6-ack-unwritable")
            .await
            .expect("inspect"),
        None,
        "a failed ack write must not leave a partial durable row"
    );

    // Nothing is gated: a thread with no installed-pending residual does not
    // block the next request or rollout regeneration because a receipt is
    // missing.
    assert_eq!(
        host_validation_reload_block(thread_id, Some(&root)).await,
        None,
        "a receipt that could not be written must not become a gate"
    );

    // The thread the failed writes targeted is untouched and still readable.
    let control = inspect_mid_turn_host_validation(thread_id, Some(&root), "cxs6-ack-control")
        .await
        .expect("inspect control")
        .expect("control row");
    assert_eq!(control.status, HostValidationStatus::Ok);
}
