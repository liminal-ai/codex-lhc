use codex_install_context::parse_lhc_release;

/// Compare fork releases (`major.minor.patch[-lhc.revision]`, bare revision 0).
pub(crate) fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_lhc_release(latest), parse_lhc_release(current)) {
        (Some(l), Some(c)) => Some(l > c),
        _ => None,
    }
}

/// Fork releases are tagged `v<release>`.
pub(crate) fn extract_version_from_latest_tag(latest_tag_name: &str) -> anyhow::Result<String> {
    latest_tag_name
        .strip_prefix('v')
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse latest tag name '{latest_tag_name}'"))
}

pub(crate) fn is_source_build_version(version: &str) -> bool {
    parse_version(version) == Some((0, 0, 0))
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut iter = v.trim().split('.');
    let maj = iter.next()?.parse::<u64>().ok()?;
    let min = iter.next()?.parse::<u64>().ok()?;
    let pat = iter.next()?.parse::<u64>().ok()?;
    Some((maj, min, pat))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn extracts_version_from_latest_tag() {
        assert_eq!(
            extract_version_from_latest_tag("v0.153.3-lhc.1").expect("failed to parse version"),
            "0.153.3-lhc.1"
        );
        assert_eq!(
            extract_version_from_latest_tag("v1.5.0").expect("failed to parse version"),
            "1.5.0"
        );
    }

    #[test]
    fn latest_tag_without_v_prefix_is_invalid() {
        for tag in ["rust-v1.5.0", "1.5.0"] {
            assert!(extract_version_from_latest_tag(tag).is_err(), "{tag:?}");
        }
    }

    #[test]
    fn fork_revision_and_upstream_base_increases_are_newer() {
        assert_eq!(is_newer("0.153.3-lhc.1", "0.153.3"), Some(true));
        assert_eq!(is_newer("0.153.3-lhc.2", "0.153.3-lhc.1"), Some(true));
        assert_eq!(is_newer("0.153.4", "0.153.3-lhc.9"), Some(true));
        assert_eq!(is_newer("0.153.3", "0.153.3-lhc.1"), Some(false));
        assert_eq!(is_newer("0.153.3-lhc.1", "0.153.3-lhc.1"), Some(false));
    }

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), None);
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), None);
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    #[test]
    fn source_build_version_is_not_checked() {
        assert!(is_source_build_version("0.0.0"));
        assert!(!is_source_build_version("0.1.0"));
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(parse_version(" 1.2.3 \n"), Some((1, 2, 3)));
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }
}
