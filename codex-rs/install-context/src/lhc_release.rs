//! Fork release identity: the `lhc-release/VERSION` value embedded in the binary
//! and the `major.minor.patch-lhc.revision` ordering used by release lookups.

/// The fork release recorded in `lhc-release/VERSION`, without trailing whitespace.
pub const LHC_RELEASE_VERSION: &str = include_str!("../../../lhc-release/VERSION").trim_ascii();

/// The GitHub API endpoint describing the fork's latest published release.
pub const LHC_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/liminal-ai/codex-lhc/releases/latest";

/// The fork's latest release page: notes, checksums, packages, and installers.
pub const LHC_RELEASES_URL: &str = "https://github.com/liminal-ai/codex-lhc/releases/latest";

/// The fork's install and update instructions.
pub const LHC_INSTALL_DOCS_URL: &str =
    "https://github.com/liminal-ai/codex-lhc/blob/main/lhc-docs/INSTALL.md";

/// The latest-release download URL of the fork's Unix installer.
pub const LHC_INSTALLER_URL_UNIX: &str =
    "https://github.com/liminal-ai/codex-lhc/releases/latest/download/install.sh";

/// The latest-release download URL of the fork's PowerShell installer.
pub const LHC_INSTALLER_URL_WINDOWS: &str =
    "https://github.com/liminal-ai/codex-lhc/releases/latest/download/install.ps1";

/// Parse a fork release (`major.minor.patch-lhc.revision`) or bare upstream
/// release (`major.minor.patch`, treated as revision 0) into a sortable tuple.
/// Any other form, including tag prefixes, returns `None`.
pub fn parse_lhc_release(release: &str) -> Option<(u64, u64, u64, u64)> {
    let (base, revision) = match release.trim().split_once("-lhc.") {
        Some((base, revision)) => (base, revision.parse::<u64>().ok()?),
        None => (release.trim(), 0),
    };
    let mut parts = base.split('.').map(str::parse::<u64>);
    let (major, minor, patch) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, revision))
}

#[cfg(test)]
#[path = "lhc_release_tests.rs"]
mod tests;
