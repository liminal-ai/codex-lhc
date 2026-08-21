use std::process::Command;

use anyhow::Result;
use pretty_assertions::assert_eq;

#[test]
fn reports_workspace_package_version() -> Result<()> {
    let output = Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .arg("--version")
        .output()?;

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?,
        format!("codex-cli {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(output.stderr, b"");
    Ok(())
}
