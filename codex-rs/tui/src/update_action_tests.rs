use super::*;
use codex_install_context::CodexPackageLayout;
use codex_install_context::InstallMethod;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;

/// The store file this platform's detection requires, and the one it ignores.
const REQUIRED_RECORD: &str = if cfg!(windows) {
    ".codex-lhc-managed"
} else {
    "installed-name"
};
const OTHER_RECORD: &str = if cfg!(windows) {
    "installed-name"
} else {
    ".codex-lhc-managed"
};

fn lhc_action(store: AbsolutePathBuf) -> UpdateAction {
    if cfg!(windows) {
        UpdateAction::LhcWindows { store }
    } else {
        UpdateAction::LhcUnix { store }
    }
}

/// The install context a binary launched from `package_dir/bin` would see.
fn package_layout_context(package_dir: &Path) -> InstallContext {
    let package_dir =
        AbsolutePathBuf::from_absolute_path(package_dir).expect("package dir should be absolute");
    InstallContext {
        method: InstallMethod::Other,
        package_layout: Some(CodexPackageLayout {
            bin_dir: package_dir.join("bin"),
            package_dir,
            resources_dir: None,
            path_dir: None,
        }),
    }
}

/// Lay out `<store>/versions/<version>` with both store records the fork
/// installers write, returning the package directory.
fn managed_package(store: &Path) -> std::path::PathBuf {
    let package_dir = store.join("versions").join("0.153.3");
    fs::create_dir_all(package_dir.join("bin")).expect("create package bin dir");
    fs::write(store.join("installed-name"), "codex-lhc\n").expect("write installed-name");
    fs::write(store.join(".codex-lhc-managed"), "managed\n").expect("write marker");
    package_dir
}

#[test]
fn managed_package_store_maps_to_lhc_installer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = temp.path().join("codex-lhc");
    let package_dir = managed_package(&store);
    // InstallContext canonicalizes the running package with std::fs, so feed
    // the same shape (verbatim-prefixed on Windows) and expect a plain store.
    let canonical_package_dir = fs::canonicalize(&package_dir).expect("canonical package dir");
    let expected_store =
        AbsolutePathBuf::from_absolute_path(dunce::canonicalize(&store).expect("canonical store"))
            .expect("store should be absolute");

    assert_eq!(
        UpdateAction::from_install_context(&package_layout_context(&canonical_package_dir)),
        Some(lhc_action(expected_store))
    );
}

#[test]
fn unmanaged_layouts_get_no_update_action() {
    let temp = tempfile::tempdir().expect("temp dir");

    // No package layout at all, whatever upstream's method detection decided.
    for method in [
        InstallMethod::Other,
        InstallMethod::Npm,
        InstallMethod::Brew,
    ] {
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method,
                package_layout: None,
            }),
            None
        );
    }

    // A package that is not under `<store>/versions/`, even with the records.
    let loose_store = temp.path().join("loose");
    let loose_package = loose_store.join("0.153.3");
    fs::create_dir_all(loose_package.join("bin")).expect("create loose package");
    fs::write(loose_store.join(REQUIRED_RECORD), "codex-lhc\n").expect("write record");
    assert_eq!(
        UpdateAction::from_install_context(&package_layout_context(&loose_package)),
        None
    );

    // The supported layout without this platform's ownership record.
    let unowned_store = temp.path().join("unowned");
    let unowned_package = managed_package(&unowned_store);
    fs::remove_file(unowned_store.join(REQUIRED_RECORD)).expect("remove required record");
    assert!(unowned_store.join(OTHER_RECORD).is_file());
    assert_eq!(
        UpdateAction::from_install_context(&package_layout_context(&unowned_package)),
        None
    );
}

#[test]
fn standalone_update_commands_rerun_latest_installer() {
    assert_eq!(
        UpdateAction::StandaloneUnix.command_args(),
        (
            "sh",
            static_args(&[
                "-c",
                "curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh"
            ]),
        )
    );
    assert_eq!(
        UpdateAction::StandaloneWindows.command_args(),
        (
            "powershell",
            static_args(&[
                "-ExecutionPolicy",
                "Bypass",
                "-c",
                "$env:CODEX_NON_INTERACTIVE=1; irm https://chatgpt.com/codex/install.ps1 | iex"
            ]),
        )
    );
}

#[cfg(unix)]
mod unix_process {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::process::ExitStatus;

    struct UpdateRun {
        status: ExitStatus,
        curl_args: Option<String>,
        installer_args: Option<String>,
        leftover_downloads: Vec<std::path::PathBuf>,
        downloads_dir: std::path::PathBuf,
    }

    fn write_script(path: &Path, body: &str) {
        fs::write(path, body).expect("write script");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    }

    /// Run the real Unix update command with `curl` replaced by a shim that
    /// records its arguments and "downloads" a recording installer. Nothing
    /// reaches the network or a real store.
    fn run_lhc_unix_update(store: &Path, curl_exit: i32, installer_exit: i32) -> UpdateRun {
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path().join("bin");
        let downloads_dir = temp.path().join("downloads");
        fs::create_dir_all(&bin).expect("create shim bin");
        fs::create_dir_all(&downloads_dir).expect("create downloads dir");
        let curl_args = temp.path().join("curl-args");
        let installer = temp.path().join("installer.sh");
        let installer_args = temp.path().join("installer-args");
        write_script(
            &bin.join("curl"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > \"$CODEX_LHC_TEST_CURL_ARGS\"\n\
             [ \"$CODEX_LHC_TEST_CURL_EXIT\" -eq 0 ] || exit \"$CODEX_LHC_TEST_CURL_EXIT\"\n\
             while [ \"$#\" -gt 1 ]; do\n\
               if [ \"$1\" = -o ]; then cp \"$CODEX_LHC_TEST_INSTALLER\" \"$2\"; exit 0; fi\n\
               shift\n\
             done\n\
             exit 1\n",
        );
        write_script(
            &installer,
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > \"$CODEX_LHC_TEST_INSTALLER_ARGS\"\n\
             exit \"$CODEX_LHC_TEST_INSTALLER_EXIT\"\n",
        );

        let action = UpdateAction::LhcUnix {
            store: AbsolutePathBuf::from_absolute_path(store).expect("store should be absolute"),
        };
        let (command, args) = action.command_args();
        let status = Command::new(command)
            .args(&args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("TMPDIR", &downloads_dir)
            .env("CODEX_LHC_TEST_CURL_ARGS", &curl_args)
            .env("CODEX_LHC_TEST_CURL_EXIT", curl_exit.to_string())
            .env("CODEX_LHC_TEST_INSTALLER", &installer)
            .env("CODEX_LHC_TEST_INSTALLER_ARGS", &installer_args)
            .env("CODEX_LHC_TEST_INSTALLER_EXIT", installer_exit.to_string())
            .status()
            .expect("run update command");
        UpdateRun {
            status,
            curl_args: fs::read_to_string(&curl_args).ok(),
            installer_args: fs::read_to_string(&installer_args).ok(),
            leftover_downloads: fs::read_dir(&downloads_dir)
                .expect("read downloads dir")
                .map(|entry| entry.expect("downloads entry").path())
                .collect(),
            downloads_dir,
        }
    }

    #[test]
    fn lhc_unix_update_runs_downloaded_installer_against_selected_store() {
        let store = tempfile::tempdir().expect("store dir");
        let run = run_lhc_unix_update(
            store.path(),
            /*curl_exit*/ 0,
            /*installer_exit*/ 0,
        );

        assert!(run.status.success(), "status {}", run.status);
        let curl_args = run.curl_args.expect("curl should record its arguments");
        let download_prefix = format!(
            "-fsSL\n{LHC_INSTALLER_URL_UNIX}\n-o\n{}/codex-lhc-install.",
            run.downloads_dir.display()
        );
        assert!(
            curl_args.starts_with(&download_prefix),
            "curl args {curl_args:?} should start with {download_prefix:?}"
        );
        assert_eq!(
            run.installer_args,
            Some(format!("--install-root\n{}\n", store.path().display()))
        );
        assert_eq!(run.leftover_downloads, Vec::<std::path::PathBuf>::new());
    }

    #[test]
    fn lhc_unix_update_stops_when_download_fails() {
        let store = tempfile::tempdir().expect("store dir");
        let run = run_lhc_unix_update(
            store.path(),
            /*curl_exit*/ 22,
            /*installer_exit*/ 0,
        );

        assert_eq!(run.status.code(), Some(22));
        assert_eq!(run.installer_args, None);
        assert_eq!(run.leftover_downloads, Vec::<std::path::PathBuf>::new());
    }

    #[test]
    fn lhc_unix_update_reports_installer_failure() {
        let store = tempfile::tempdir().expect("store dir");
        let run = run_lhc_unix_update(
            store.path(),
            /*curl_exit*/ 0,
            /*installer_exit*/ 3,
        );

        assert_eq!(run.status.code(), Some(3));
        assert_eq!(
            run.installer_args,
            Some(format!("--install-root\n{}\n", store.path().display()))
        );
        assert_eq!(run.leftover_downloads, Vec::<std::path::PathBuf>::new());
    }
}

#[cfg(windows)]
mod windows_process {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::process::Command;
    use std::process::ExitStatus;

    struct UpdateRun {
        status: ExitStatus,
        download_url: Option<String>,
        installer_args: Option<String>,
    }

    /// Run the real Windows update script with `Invoke-WebRequest` shadowed by
    /// a function that records the URL and "downloads" a recording installer.
    /// Nothing reaches the network or a real store.
    fn run_lhc_windows_update(
        store: &Path,
        download_fail: &str,
        installer_fail: &str,
    ) -> UpdateRun {
        let temp = tempfile::tempdir().expect("temp dir");
        let installer = temp.path().join("installer.ps1");
        let download_url = temp.path().join("download-url");
        let installer_args = temp.path().join("installer-args");
        fs::write(
            &installer,
            "param([string]$InstallRoot)\r\n\
             Set-Content $env:CODEX_LHC_TEST_INSTALLER_ARGS $InstallRoot\r\n\
             if ($env:CODEX_LHC_TEST_INSTALLER_FAIL) { throw 'installer failed' }\r\n",
        )
        .expect("write installer fixture");

        let action = UpdateAction::LhcWindows {
            store: AbsolutePathBuf::from_absolute_path(store).expect("store should be absolute"),
        };
        let (command, mut args) = action.command_args();
        let script = args.pop().expect("update script");
        let shim = "function Invoke-WebRequest { param([string]$Uri, [string]$OutFile) Set-Content $env:CODEX_LHC_TEST_DOWNLOAD_URL $Uri; if ($env:CODEX_LHC_TEST_DOWNLOAD_FAIL) { throw 'download failed' }; Copy-Item $env:CODEX_LHC_TEST_INSTALLER $OutFile }; ";
        let status = Command::new(format!("{command}.exe"))
            .args(&args)
            .arg(format!("{shim}{script}"))
            .env("CODEX_LHC_TEST_INSTALLER", &installer)
            .env("CODEX_LHC_TEST_INSTALLER_ARGS", &installer_args)
            .env("CODEX_LHC_TEST_INSTALLER_FAIL", installer_fail)
            .env("CODEX_LHC_TEST_DOWNLOAD_URL", &download_url)
            .env("CODEX_LHC_TEST_DOWNLOAD_FAIL", download_fail)
            .status()
            .expect("run update command");
        UpdateRun {
            status,
            download_url: fs::read_to_string(&download_url)
                .ok()
                .map(|url| url.trim().to_string()),
            installer_args: fs::read_to_string(&installer_args)
                .ok()
                .map(|args| args.trim().to_string()),
        }
    }

    #[test]
    fn lhc_windows_update_runs_downloaded_installer_against_selected_store() {
        let store = tempfile::tempdir().expect("store dir");
        let run = run_lhc_windows_update(store.path(), "", "");

        assert!(run.status.success(), "status {}", run.status);
        assert_eq!(run.download_url.as_deref(), Some(LHC_INSTALLER_URL_WINDOWS));
        assert_eq!(run.installer_args, Some(store.path().display().to_string()));
    }

    #[test]
    fn lhc_windows_update_stops_when_download_fails() {
        let store = tempfile::tempdir().expect("store dir");
        let run = run_lhc_windows_update(store.path(), "1", "");

        assert!(!run.status.success());
        assert_eq!(run.installer_args, None);
    }

    #[test]
    fn lhc_windows_update_reports_installer_failure() {
        let store = tempfile::tempdir().expect("store dir");
        let run = run_lhc_windows_update(store.path(), "", "1");

        assert!(!run.status.success());
        assert_eq!(run.installer_args, Some(store.path().display().to_string()));
    }
}
