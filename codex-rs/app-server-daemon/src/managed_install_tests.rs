use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::executable_identity_from_bytes;
use super::managed_codex_bin;
use super::parse_codex_version;
use super::seed_managed_codex_from_running_fork;

#[test]
fn parses_codex_cli_version_output() {
    let workspace_version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        parse_codex_version(&format!("codex-cli {workspace_version}\n")).expect("version"),
        workspace_version
    );
}

#[test]
fn rejects_malformed_codex_cli_version_output() {
    assert!(parse_codex_version("codex\n").is_err());
}

#[test]
fn executable_identity_uses_binary_contents() {
    let old = executable_identity_from_bytes(b"old");
    let same = executable_identity_from_bytes(b"old");
    let new = executable_identity_from_bytes(b"new");

    assert_eq!(old, same);
    assert_ne!(old, new);
}

#[test]
fn seed_managed_codex_copies_running_fork_bytes_into_isolated_home() {
    let home = TempDir::new().expect("temp dir");
    let managed = managed_codex_bin(home.path());
    assert!(!managed.is_file());

    seed_managed_codex_from_running_fork(&managed).expect("seed fork bytes");

    assert!(managed.is_file());
    let source = std::env::current_exe().expect("current exe");
    let expected = std::fs::read(&source).expect("read source");
    let actual = std::fs::read(&managed).expect("read seeded");
    assert_eq!(actual, expected, "seeded bytes must match the running fork");
    assert_ne!(managed, source);
}
