//! Diagnoses whether Codex update paths target the running installation.
//!
//! Update diagnostics combine cached version metadata, install-channel hints,
//! and bounded latest-version probes. For npm-managed launches, this module also
//! verifies that npm install -g would update the package root that launched the
//! current process, which catches PATH and prefix mismatches before the user runs
//! an update command.

use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::time::Duration;

use codex_core::config::Config;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use codex_http_client::ClientRouteClass;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use codex_http_client::RouteAwareClientPool;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use codex_install_context::LHC_INSTALL_DOCS_URL;
use codex_install_context::LHC_LATEST_RELEASE_API_URL;
use codex_install_context::LHC_RELEASE_VERSION;
use codex_install_context::parse_lhc_release;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use http::Method;
use serde::Deserialize;
#[cfg(target_os = "macos")]
use url::Url;

use super::CheckStatus;
use super::DoctorCheck;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use super::DoctorIssue;
use super::NpmRootCheck;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use super::desktop::platform::InstalledApp;
use super::doctor_install_context;
use super::doctor_managed_by_npm;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use super::network;
use super::npm_global_root_check;
use super::run_command;

/// The TUI's fork-owned cache; stock `version.json` is not inspected.
const VERSION_FILE_NAME: &str = "lhc-version.json";
const HOMEBREW_CASK_API_URL: &str = "https://formulae.brew.sh/api/cask/codex.json";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const DESKTOP_UPDATE_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/appcast-x64.xml";
#[cfg(all(target_os = "macos", not(target_arch = "x86_64")))]
const DESKTOP_UPDATE_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/appcast.xml";
#[cfg(target_os = "macos")]
const BACKEND_DESKTOP_UPDATE_URL: &str = "https://chatgpt.com/backend-api/wham/app/appcast";
#[cfg(target_os = "windows")]
const DESKTOP_UPDATE_URL: &str =
    "https://persistent.oaistatic.com/codex-app-prod/windows-store-update.json";

/// Builds the update-health row for the current installation.
///
/// Network failures while fetching latest-version metadata degrade the row to a
/// warning instead of failing doctor outright; update freshness is useful
/// support context but should not mask more direct install/config failures.
pub(super) fn updates_check(config: &Config) -> DoctorCheck {
    let current_exe = std::env::current_exe().ok();
    let install_context = doctor_install_context(current_exe.as_deref());
    let mut details = vec![
        format!(
            "check for update on startup: {}",
            config.check_for_update_on_startup
        ),
        format!("running fork release: {LHC_RELEASE_VERSION}"),
        "update action: installer-managed release builds run this executable with `update`"
            .to_string(),
        format!("manual update: {LHC_INSTALL_DOCS_URL}"),
    ];
    let version_file = config.codex_home.join(VERSION_FILE_NAME);
    push_cached_version_details(&mut details, &version_file);

    let mut status = CheckStatus::Ok;
    let mut summary = "update configuration is locally consistent".to_string();
    let mut remediation = None;

    if doctor_managed_by_npm(current_exe.as_deref()) {
        match npm_global_root_check() {
            NpmRootCheck::Match { package_root } => {
                details.push(format!("npm update target: {}", package_root.display()));
            }
            NpmRootCheck::Mismatch {
                running_package_root,
                npm_package_root,
            } => {
                status = CheckStatus::Fail;
                summary = "update would target a different npm install".to_string();
                details.push(format!(
                    "running package root: {}",
                    running_package_root.display()
                ));
                details.push(format!("npm package root: {}", npm_package_root.display()));
                remediation = Some(format!(
                    "Fix PATH or npm prefix so the running package root ({}) matches the npm global package root ({}).",
                    running_package_root.display(),
                    npm_package_root.display()
                ));
            }
            NpmRootCheck::MissingPackageRoot => {
                status = status.max(CheckStatus::Warning);
                summary = "npm update target could not be proven".to_string();
                remediation = Some(
                    "Reinstall or update Codex so the JS shim provides CODEX_MANAGED_PACKAGE_ROOT."
                        .to_string(),
                );
            }
            NpmRootCheck::NpmUnavailable(error) => {
                status = status.max(CheckStatus::Warning);
                summary = "npm update target could not be inspected".to_string();
                details.push(format!("npm root -g failed: {error}"));
            }
        }
    }

    match fetch_latest_version(&install_context) {
        Ok(latest_version) => {
            details.push(format!("latest version: {latest_version}"));
            if is_newer_fork_release(&latest_version) {
                details.push("latest version status: newer version is available".to_string());
            } else {
                details.push("latest version status: current version is not older".to_string());
            }
        }
        Err(err) => {
            status = status.max(CheckStatus::Warning);
            details.push(format!("latest version probe: {err}"));
        }
    }

    let mut check = DoctorCheck::new("updates.status", "updates", status, summary).details(details);
    if let Some(remediation) = remediation {
        check = check.remediation(remediation);
    }
    check
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(super) async fn append_desktop_update(
    checks: &mut [DoctorCheck],
    config: Option<&Config>,
    application: &InstalledApp,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Some(build) = latest_macos_staged_build(
            &home
                .join("Library/Caches")
                .join(application.identity)
                .join("org.sparkle-project.Sparkle/Installation"),
            application.build,
        )
        .await
        && let Some(update) = checks.iter_mut().find(|check| check.id == "updates.status")
    {
        update.details.extend([
            "desktop update status: ready to install".to_string(),
            format!("desktop latest build: {build}"),
            format!("desktop application: {}", application.identity),
        ]);
    }

    let Some(config) = config else {
        return;
    };
    let Some(reachability_index) = checks
        .iter()
        .position(|check| check.id == "network.provider_reachability")
    else {
        return;
    };
    #[cfg(target_os = "macos")]
    let desktop_update_url = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| {
            macos_desktop_update_url(&home, application, &os_info::get().version().to_string())
        })
        .unwrap_or_else(|| DESKTOP_UPDATE_URL.to_string());
    #[cfg(target_os = "windows")]
    let desktop_update_url = DESKTOP_UPDATE_URL;
    #[cfg(target_os = "macos")]
    let desktop_update_url = desktop_update_url.as_str();
    let desktop_update_display_url = desktop_update_url
        .split_once('?')
        .map_or(desktop_update_url, |(endpoint, _)| endpoint);
    let client = RouteAwareClientPool::new_without_request_logging(
        config.http_client_factory(),
        ClientRouteClass::Other,
    );
    let outcome = match client
        .request(Method::GET, desktop_update_url)
        .timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status().as_u16();
            #[cfg(target_os = "windows")]
            if status == 404 && response.url().scheme() == "https" {
                checks[reachability_index].details.push(format!(
                    "desktop assets CDN: {desktop_update_display_url} reachable (HTTP 404; no update available)"
                ));
                return;
            }
            if cfg!(target_os = "windows") && response.url().scheme() != "https" {
                Err("update manifest redirected to a non-HTTPS URL".to_string())
            } else if status == 407 {
                Err("proxy authentication required (HTTP 407)".to_string())
            } else if !(200..=299).contains(&status) {
                Err(format!("HTTP {status}"))
            } else {
                checks[reachability_index].details.push(format!(
                    "desktop assets CDN: {desktop_update_display_url} reachable (HTTP {status})"
                ));
                #[cfg(target_os = "windows")]
                if let Some(update) = checks.iter_mut().find(|check| check.id == "updates.status") {
                    match response.bytes().await {
                        Ok(body) => match windows_store_update(&body, &application.version) {
                            Ok(Some(build)) => update.details.extend([
                                "desktop update status: available".to_string(),
                                format!("desktop latest build: {build}"),
                                format!("desktop application: {}", application.identity),
                            ]),
                            Ok(None) => {}
                            Err(error) => {
                                update.status = update.status.max(CheckStatus::Warning);
                                update
                                    .details
                                    .push(format!("desktop update manifest: {error}"));
                            }
                        },
                        Err(_) => {
                            update.status = update.status.max(CheckStatus::Warning);
                            update
                                .details
                                .push("desktop update manifest: response could not be read".into());
                        }
                    }
                }
                Ok(())
            }
        }
        Err(error) => Err(network::request_error(error)),
    };

    if let Err(error) = outcome {
        let reachability = &mut checks[reachability_index];
        reachability.details.push(format!(
            "desktop assets CDN: {desktop_update_display_url} {error} (optional)"
        ));
        if reachability.status == CheckStatus::Ok {
            reachability.status = CheckStatus::Warning;
            reachability.summary = "desktop update and runtime CDN is unreachable".to_string();
        }
        reachability.issues.push(
            DoctorIssue::new(
                CheckStatus::Warning,
                "desktop update and runtime CDN is unreachable",
            )
            .measured(format!("{desktop_update_display_url} {error}"))
            .expected("desktop update and runtime CDN reachable over HTTPS")
            .remedy(
                if desktop_update_display_url.starts_with("https://chatgpt.com/") {
                    "check proxy, firewall, DNS, and certificate access to chatgpt.com"
                } else {
                    "check proxy, firewall, DNS, and certificate access to persistent.oaistatic.com"
                },
            )
            .field("desktop assets CDN"),
        );
    }
}

#[cfg(target_os = "macos")]
fn macos_desktop_update_url(home: &Path, application: &InstalledApp, os_version: &str) -> String {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ProductionAppcastState {
        #[serde(default)]
        backend_appcast_enabled: bool,
        installation_id: Option<String>,
    }

    let state_path = home
        .join("Library/Application Support")
        .join(application.identity)
        .join("production-appcast-bootstrap.json");
    let Some(state) = std::fs::read(state_path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<ProductionAppcastState>(&contents).ok())
    else {
        return DESKTOP_UPDATE_URL.to_string();
    };
    let Some(installation_id) = state
        .backend_appcast_enabled
        .then_some(state.installation_id)
        .flatten()
    else {
        return DESKTOP_UPDATE_URL.to_string();
    };

    let Ok(mut url) = Url::parse(BACKEND_DESKTOP_UPDATE_URL) else {
        return DESKTOP_UPDATE_URL.to_string();
    };
    url.query_pairs_mut().extend_pairs([
        ("installation_id", installation_id.as_str()),
        (
            "arch",
            if cfg!(target_arch = "x86_64") {
                "x64"
            } else {
                "arm64"
            },
        ),
        ("app_version", application.version.as_str()),
        ("beta", "false"),
        ("os-version", os_version),
        ("plan_type", "unknown"),
    ]);
    url.to_string()
}

#[cfg(any(target_os = "windows", test))]
fn windows_store_update(
    manifest: &[u8],
    installed_version: &str,
) -> Result<Option<String>, &'static str> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StoreManifest {
        schema_version: u64,
        build_version: String,
        store_product_id: String,
        package_identity: String,
    }

    let manifest: StoreManifest =
        serde_json::from_slice(manifest).map_err(|_| "invalid Windows Store update manifest")?;
    if manifest.schema_version == 0
        || manifest.store_product_id != "9PLM9XGG6VKS"
        || manifest.package_identity != "OpenAI.Codex"
    {
        return Err("Windows Store update manifest does not target the production application");
    }
    let version = |value: &str| -> Option<[u64; 4]> {
        value
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?
            .try_into()
            .ok()
    };
    let latest = version(&manifest.build_version)
        .ok_or("Windows Store update manifest contains an invalid build version")?;
    let installed =
        version(installed_version).ok_or("installed Windows application has an invalid version")?;
    Ok((latest > installed).then_some(manifest.build_version))
}

#[cfg(target_os = "macos")]
async fn latest_macos_staged_build(root: &Path, installed_build: u64) -> Option<u64> {
    const MAX_STAGED_BUNDLES: usize = 64;

    if !std::fs::symlink_metadata(root).ok()?.is_dir() {
        return None;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut inspected = 0;
    let mut latest = None;
    for entry in std::fs::read_dir(root).ok()? {
        if inspected == MAX_STAGED_BUNDLES || tokio::time::Instant::now() >= deadline {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let extracted = entry.path().join("extracted");
        if !std::fs::symlink_metadata(&extracted).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        let bundle = extracted.join("ChatGPT.app");
        if !std::fs::symlink_metadata(&bundle).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        inspected += 1;
        let Ok(result) = tokio::time::timeout_at(
            deadline,
            super::desktop::platform::inspect_macos_bundle(&bundle),
        )
        .await
        else {
            break;
        };
        if let Ok(Some(application)) = result
            && application.build > installed_build
        {
            latest = Some(latest.map_or(application.build, |latest: u64| {
                latest.max(application.build)
            }));
        }
    }
    latest
}

fn push_cached_version_details(details: &mut Vec<String>, version_file: &Path) {
    details.push(format!("version cache: {}", version_file.display()));
    match std::fs::read_to_string(version_file) {
        Ok(contents) => match serde_json::from_str::<VersionInfo>(&contents) {
            Ok(info) => {
                details.push(format!("cached latest version: {}", info.latest_version));
                if let Some(last_checked_at) = info.last_checked_at {
                    details.push(format!("last checked at: {last_checked_at}"));
                }
                if let Some(dismissed_version) = info.dismissed_version {
                    details.push(format!("dismissed version: {dismissed_version}"));
                }
            }
            Err(err) => details.push(format!("version cache parse: {err}")),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            details.push("version cache: missing".to_string());
        }
        Err(err) => details.push(format!("version cache read: {err}")),
    }
}

fn fetch_latest_version(context: &InstallContext) -> Result<String, String> {
    match &context.method {
        InstallMethod::Brew => fetch_homebrew_cask_version(),
        InstallMethod::Npm
        | InstallMethod::Bun
        | InstallMethod::VitePlus
        | InstallMethod::Pnpm
        | InstallMethod::Standalone { .. }
        | InstallMethod::Other => fetch_latest_github_release_version(),
    }
}

fn fetch_latest_github_release_version() -> Result<String, String> {
    #[derive(Deserialize)]
    struct ReleaseInfo {
        tag_name: String,
    }

    let info = http_get_json::<ReleaseInfo>(LHC_LATEST_RELEASE_API_URL)?;
    release_from_tag(&info.tag_name)
}

/// Fork releases are tagged `v<release>`.
fn release_from_tag(tag_name: &str) -> Result<String, String> {
    tag_name
        .strip_prefix('v')
        .map(str::to_string)
        .ok_or_else(|| format!("failed to parse latest tag {tag_name}"))
}

fn fetch_homebrew_cask_version() -> Result<String, String> {
    #[derive(Deserialize)]
    struct HomebrewCaskInfo {
        version: String,
    }

    http_get_json::<HomebrewCaskInfo>(HOMEBREW_CASK_API_URL).map(|info| info.version)
}

fn http_get_json<T>(url: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let body = run_command("curl", ["-fsSL", "--max-time", "5", url])?;
    serde_json::from_str::<T>(&body).map_err(|err| err.to_string())
}

/// Whether `latest` is a newer fork release than the running one
/// (`major.minor.patch[-lhc.revision]`, bare revision 0).
fn is_newer_fork_release(latest: &str) -> bool {
    matches!(
        (parse_lhc_release(latest), parse_lhc_release(LHC_RELEASE_VERSION)),
        (Some(latest), Some(current)) if latest > current
    )
}

#[derive(Deserialize)]
struct VersionInfo {
    latest_version: String,
    #[serde(default)]
    last_checked_at: Option<String>,
    #[serde(default)]
    dismissed_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_update_probe_uses_the_persisted_production_appcast_feed() {
        let home = tempfile::tempdir().expect("temporary home should be created");
        let application = InstalledApp {
            identity: "com.openai.codex",
            version: "26.623.10000".to_string(),
            bundle: PathBuf::new(),
            build: 6139,
        };
        assert_eq!(
            macos_desktop_update_url(home.path(), &application, "26.6.0"),
            DESKTOP_UPDATE_URL
        );

        let state_directory = home
            .path()
            .join("Library/Application Support/com.openai.codex");
        std::fs::create_dir_all(&state_directory)
            .expect("production appcast state directory should be created");
        std::fs::write(
            state_directory.join("production-appcast-bootstrap.json"),
            r#"{"backendAppcastEnabled":true,"installationId":"028e90f8-5f2a-47db-a05c-6a48f548d728"}"#,
        )
        .expect("production appcast state should be created");

        let arch = if cfg!(target_arch = "x86_64") {
            "x64"
        } else {
            "arm64"
        };
        assert_eq!(
            macos_desktop_update_url(home.path(), &application, "26.6.0"),
            format!(
                "{BACKEND_DESKTOP_UPDATE_URL}?installation_id=028e90f8-5f2a-47db-a05c-6a48f548d728&arch={arch}&app_version=26.623.10000&beta=false&os-version=26.6.0&plan_type=unknown"
            )
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_staged_updates_require_a_newer_matching_extracted_bundle() {
        let root = tempfile::tempdir().expect("temporary Sparkle cache should be created");
        for index in 0..320 {
            std::fs::create_dir(root.path().join(format!("unrelated-{index}")))
                .expect("unrelated Sparkle cache directory should be created");
        }
        for (name, identity, build) in [
            ("newest", "com.openai.codex", "6268"),
            ("newer", "com.openai.codex", "6168"),
            ("older", "com.openai.codex", "6138"),
            ("different", "com.example.other", "9999"),
            ("invalid", "com.openai.codex", "invalid"),
        ] {
            let bundle = root.path().join(name).join("extracted/ChatGPT.app");
            write_macos_bundle(&bundle, identity, build);
        }
        let outside = tempfile::tempdir().expect("external fixture should be created");
        let linked = outside.path().join("ChatGPT.app");
        write_macos_bundle(&linked, "com.openai.codex", "9999");
        std::os::unix::fs::symlink(&linked, root.path().join("ChatGPT.app"))
            .expect("symlinked staged app fixture should be created");

        assert_eq!(
            latest_macos_staged_build(root.path(), /*installed_build*/ 6139).await,
            Some(6268)
        );
        assert_eq!(
            latest_macos_staged_build(root.path(), /*installed_build*/ 6268).await,
            None
        );
    }

    #[test]
    fn windows_store_updates_compare_all_four_production_build_components() {
        let mut manifest = serde_json::json!({
            "schemaVersion": 1,
            "buildVersion": "26.803.5235.1",
            "storeProductId": "9PLM9XGG6VKS",
            "packageIdentity": "OpenAI.Codex",
        });
        assert_eq!(
            windows_store_update(&serde_json::to_vec(&manifest).unwrap(), "26.803.5235.0"),
            Ok(Some("26.803.5235.1".to_string()))
        );
        assert_eq!(
            windows_store_update(&serde_json::to_vec(&manifest).unwrap(), "26.803.5235.1"),
            Ok(None)
        );
        manifest["storeProductId"] = "other".into();
        assert!(
            windows_store_update(&serde_json::to_vec(&manifest).unwrap(), "26.803.5235.0").is_err()
        );
    }

    #[cfg(target_os = "macos")]
    fn write_macos_bundle(path: &Path, identity: &str, build: &str) {
        let contents = path.join("Contents");
        std::fs::create_dir_all(&contents).expect("staged app fixture should be created");
        std::fs::write(
            contents.join("Info.plist"),
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
                 <key>CFBundleIdentifier</key><string>{identity}</string>\
                 <key>CFBundleVersion</key><string>{build}</string>\
                 </dict></plist>"
            ),
        )
        .expect("staged app metadata should be created");
    }

    #[test]
    fn release_tags_drop_the_fork_prefix() {
        assert_eq!(
            release_from_tag("v0.153.3-lhc.1"),
            Ok("0.153.3-lhc.1".to_string())
        );
        assert_eq!(release_from_tag("v0.154.0"), Ok("0.154.0".to_string()));
        assert!(release_from_tag("rust-v0.153.3").is_err());
    }

    #[test]
    fn newer_fork_releases_beat_the_running_release_by_revision_or_base() {
        let (major, minor, patch, revision) =
            parse_lhc_release(LHC_RELEASE_VERSION).expect("embedded release should parse");
        let next_revision = format!("{major}.{minor}.{patch}-lhc.{}", revision + 1);
        let next_base = format!("{major}.{minor}.{}", patch + 1);

        assert!(is_newer_fork_release(&next_revision));
        assert!(is_newer_fork_release(&next_base));
        assert!(!is_newer_fork_release(LHC_RELEASE_VERSION));
        assert!(!is_newer_fork_release(&format!("{major}.{minor}.{patch}")));
        assert!(!is_newer_fork_release("0.153.3-beta.1"));
    }

    #[test]
    fn cached_details_come_from_the_fork_cache_not_the_stock_file() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home should be created");
        std::fs::write(
            codex_home.path().join("version.json"),
            r#"{"latest_version":"9.9.9","dismissed_version":"9.9.8"}"#,
        )
        .expect("stock cache should be written");
        std::fs::write(
            codex_home.path().join(VERSION_FILE_NAME),
            r#"{"latest_version":"0.153.3-lhc.2","last_checked_at":"2026-09-06T00:00:00Z","dismissed_version":"0.153.3-lhc.1"}"#,
        )
        .expect("fork cache should be written");
        let version_file = codex_home.path().join(VERSION_FILE_NAME);

        let mut details = Vec::new();
        push_cached_version_details(&mut details, &version_file);

        assert_eq!(
            details,
            vec![
                format!("version cache: {}", version_file.display()),
                "cached latest version: 0.153.3-lhc.2".to_string(),
                "last checked at: 2026-09-06T00:00:00Z".to_string(),
                "dismissed version: 0.153.3-lhc.1".to_string(),
            ]
        );
    }
}
