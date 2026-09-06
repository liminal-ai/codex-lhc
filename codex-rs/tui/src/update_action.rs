#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallContext;
use codex_install_context::LHC_INSTALLER_URL_UNIX;
use codex_install_context::LHC_INSTALLER_URL_WINDOWS;
use codex_utils_absolute_path::AbsolutePathBuf;
#[cfg(any(not(debug_assertions), test))]
use std::ffi::OsStr;

/// Update action the CLI should perform after the TUI exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    /// Update via `npm install -g @openai/codex@latest`.
    NpmGlobalLatest,
    /// Update via `bun install -g @openai/codex@latest`.
    BunGlobalLatest,
    /// Update via `vp install -g @openai/codex@latest`.
    VitePlusGlobalLatest,
    /// Update via `pnpm add -g @openai/codex@latest`.
    PnpmGlobalLatest,
    /// Update via `brew upgrade codex`.
    BrewUpgrade,
    /// Update via `curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh`.
    StandaloneUnix,
    /// Update via `$env:CODEX_NON_INTERACTIVE=1; irm https://chatgpt.com/codex/install.ps1 | iex`.
    StandaloneWindows,
    /// Update via the fork's `install.sh` against the managed package store.
    /// The installer reads the recorded command name and prefix from the store.
    LhcUnix { store: AbsolutePathBuf },
    /// Update via the fork's `install.ps1` against the managed package store.
    LhcWindows { store: AbsolutePathBuf },
}

impl UpdateAction {
    /// The fork updates one install shape only: the running package is
    /// `<store>/versions/<version>` and the store carries the installer's
    /// ownership record. Every other install gets manual instructions.
    #[cfg(any(not(debug_assertions), test))]
    pub(crate) fn from_install_context(context: &InstallContext) -> Option<Self> {
        let package_dir = &context.package_layout.as_ref()?.package_dir;
        let versions_dir = package_dir.parent()?;
        if versions_dir.file_name() != Some(OsStr::new("versions")) {
            return None;
        }
        // The layout was canonicalized with std::fs, which on Windows keeps a
        // verbatim `\\?\` prefix that neither the PowerShell installer nor the
        // launcher it writes can use.
        let store = AbsolutePathBuf::from_absolute_path(dunce::simplified(
            versions_dir.parent()?.as_path(),
        ))
        .ok()?;
        let ownership_record = if cfg!(windows) {
            ".codex-lhc-managed"
        } else {
            "installed-name"
        };
        if !store.join(ownership_record).is_file() {
            return None;
        }
        Some(if cfg!(windows) {
            UpdateAction::LhcWindows { store }
        } else {
            UpdateAction::LhcUnix { store }
        })
    }

    /// Returns the command and arguments for invoking the update.
    pub fn command_args(&self) -> (&'static str, Vec<String>) {
        match self {
            UpdateAction::NpmGlobalLatest => {
                ("npm", static_args(&["install", "-g", "@openai/codex"]))
            }
            UpdateAction::BunGlobalLatest => {
                ("bun", static_args(&["install", "-g", "@openai/codex"]))
            }
            UpdateAction::VitePlusGlobalLatest => {
                ("vp", static_args(&["install", "-g", "@openai/codex"]))
            }
            UpdateAction::PnpmGlobalLatest => {
                ("pnpm", static_args(&["add", "-g", "@openai/codex"]))
            }
            UpdateAction::BrewUpgrade => ("brew", static_args(&["upgrade", "--cask", "codex"])),
            UpdateAction::StandaloneUnix => (
                "sh",
                static_args(&[
                    "-c",
                    "curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh",
                ]),
            ),
            UpdateAction::StandaloneWindows => (
                "powershell",
                static_args(&[
                    "-ExecutionPolicy",
                    "Bypass",
                    "-c",
                    "$env:CODEX_NON_INTERACTIVE=1; irm https://chatgpt.com/codex/install.ps1 | iex",
                ]),
            ),
            // Download to a file first: a failed download then stops the
            // update instead of piping a partial script into the shell. The
            // store travels as `$1` so its path is never re-parsed, and the
            // script avoids single quotes so its shell-quoted display stays
            // readable.
            UpdateAction::LhcUnix { store } => (
                "sh",
                vec![
                    "-c".to_string(),
                    format!(
                        "f=$(mktemp \"${{TMPDIR:-/tmp}}/codex-lhc-install.XXXXXX\") && curl -fsSL {LHC_INSTALLER_URL_UNIX} -o \"$f\" && sh \"$f\" --install-root \"$1\"; s=$?; rm -f \"$f\"; exit $s"
                    ),
                    "sh".to_string(),
                    store.to_string_lossy().into_owned(),
                ],
            ),
            UpdateAction::LhcWindows { store } => {
                let store = store.to_string_lossy().replace('\'', "''");
                (
                    "powershell",
                    vec![
                        "-ExecutionPolicy".to_string(),
                        "Bypass".to_string(),
                        "-c".to_string(),
                        format!(
                            "$ErrorActionPreference = 'Stop'; $installer = Join-Path ([System.IO.Path]::GetTempPath()) ('codex-lhc-install-' + [guid]::NewGuid() + '.ps1'); try {{ Invoke-WebRequest '{LHC_INSTALLER_URL_WINDOWS}' -OutFile $installer; & $installer -InstallRoot '{store}' }} finally {{ Remove-Item $installer -Force -ErrorAction SilentlyContinue }}"
                        ),
                    ],
                )
            }
        }
    }

    /// Returns string representation of the command-line arguments for invoking the update.
    pub fn command_str(&self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command).chain(args.iter().map(String::as_str)))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }
}

fn static_args(args: &[&str]) -> Vec<String> {
    args.iter().map(ToString::to_string).collect()
}

#[cfg(not(debug_assertions))]
pub fn get_update_action() -> Option<UpdateAction> {
    UpdateAction::from_install_context(InstallContext::current())
}

#[cfg(test)]
#[path = "update_action_tests.rs"]
mod tests;
