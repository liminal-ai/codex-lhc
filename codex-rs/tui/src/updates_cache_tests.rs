use super::*;
use crate::legacy_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[tokio::test]
async fn dismiss_version_creates_cache_file_when_missing() {
    let codex_home = tempdir().expect("temp codex home");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let version_file = version_filepath(&config);

    dismiss_version(&config, "999.0.0")
        .await
        .expect("dismiss version");

    let info = read_version_info(&version_file).expect("read version info");
    assert_eq!(info.last_checked_at, DateTime::<Utc>::UNIX_EPOCH);
    assert_eq!(
        (
            info.latest_version.as_str(),
            info.dismissed_version.as_deref()
        ),
        ("999.0.0", Some("999.0.0"))
    );
}

#[tokio::test]
async fn fork_cache_leaves_stock_version_file_untouched() {
    let codex_home = tempdir().expect("temp codex home");
    let stock_file = codex_home.path().join("version.json");
    let stock_contents =
        "{\"latest_version\":\"0.1.0\",\"last_checked_at\":\"2026-01-01T00:00:00Z\"}\n";
    std::fs::write(&stock_file, stock_contents).expect("write stock cache");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let version_file = version_filepath(&config);

    dismiss_version(&config, "0.153.3-lhc.1")
        .await
        .expect("dismiss version");

    assert_eq!(version_file, codex_home.path().join("lhc-version.json"));
    assert_eq!(
        std::fs::read_to_string(&stock_file).expect("read stock cache"),
        stock_contents
    );
    assert_eq!(
        read_version_info(&version_file)
            .expect("read fork cache")
            .dismissed_version
            .as_deref(),
        Some("0.153.3-lhc.1")
    );
}
