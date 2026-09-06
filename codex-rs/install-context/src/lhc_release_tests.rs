use super::*;
use pretty_assertions::assert_eq;

#[test]
fn orders_by_base_then_revision() {
    assert_eq!(parse_lhc_release("0.153.3-lhc.2"), Some((0, 153, 3, 2)));
    assert!(parse_lhc_release("0.153.3-lhc.2") > parse_lhc_release("0.153.3-lhc.1"));
    assert!(parse_lhc_release("0.154.0-lhc.1") > parse_lhc_release("0.153.3-lhc.9"));
    assert!(parse_lhc_release("1.0.0") > parse_lhc_release("0.153.3-lhc.9"));
}

#[test]
fn bare_release_is_revision_zero() {
    assert_eq!(parse_lhc_release("0.153.3\n"), Some((0, 153, 3, 0)));
    assert!(parse_lhc_release("0.153.3-lhc.1") > parse_lhc_release("0.153.3"));
}

#[test]
fn rejects_unsupported_forms() {
    for release in [
        "v0.153.3",
        "rust-v0.153.3",
        "0.153",
        "0.153.3.4",
        "0.153.3-lhc",
        "0.153.3-lhc.x",
        "0.153.3-beta.1",
        "",
    ] {
        assert_eq!(parse_lhc_release(release), None, "{release:?}");
    }
}
