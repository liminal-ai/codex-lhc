//! Typed worker-boundary failures. Diagnostic wording never selects recovery policy.

use codex_lhc_host::LhcCompactUnavailable;

#[derive(Debug)]
pub(super) enum CompactWorkerError {
    Cancelled(String),
    TimedOut(String),
    Worker(String),
    Operation(String),
    Produce(LhcCompactUnavailable),
}

impl From<LhcCompactUnavailable> for CompactWorkerError {
    fn from(error: LhcCompactUnavailable) -> Self {
        match error {
            LhcCompactUnavailable::Cancelled => Self::Cancelled(error.to_string()),
            error @ (LhcCompactUnavailable::OpenFailed(_)
            | LhcCompactUnavailable::NoEvents
            | LhcCompactUnavailable::ArchiveDoesNotCoverHost(_)
            | LhcCompactUnavailable::CompactFailed(_)
            | LhcCompactUnavailable::EmptyView(_)
            | LhcCompactUnavailable::ViewFetchFailed(_)
            | LhcCompactUnavailable::Inference(_)
            | LhcCompactUnavailable::DerivationFailed(_)
            | LhcCompactUnavailable::NoReduction(_)) => Self::Produce(error),
        }
    }
}

impl std::fmt::Display for CompactWorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled(message)
            | Self::TimedOut(message)
            | Self::Worker(message)
            | Self::Operation(message) => f.write_str(message),
            Self::Produce(error) => error.fmt(f),
        }
    }
}
