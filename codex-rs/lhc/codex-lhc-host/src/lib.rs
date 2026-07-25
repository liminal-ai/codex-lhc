//! LHC host adapter for Codex — Chunk 0 skeleton.
//!
//! Fork-only crate; carries no host hooks itself. The host-side touchpoints
//! that call into this crate are enumerated in FORK.md and marked with
//! `LHC-HOOK` sentinels. Laws binding this adapter (Phase 3 scar tissue,
//! recorded in the Phase 4 brief):
//! - write-back is the architecture: after an LHC compact, host state IS
//!   the LHC body (via `Session::replace_compacted_history`);
//! - bands may compress to text; the live tail conserves host-native kinds;
//! - classify on the typed session view (source linkage, variants, ids) —
//!   never reconstruct structure from rendered text;
//! - per-entry classification fails toward synthetic; whole-index
//!   construction failure fails the operation;
//! - fixtures must be shapes the host can actually produce, and a test
//!   that cannot fail is not a test.

/// Linkage proof through a real, behavior-bearing port export: the
/// JS-parity serializer (the port's most load-bearing shared surface).
pub fn lhc_port_linked() -> String {
    lhc::shared_tech::js_json::js_json_stringify(&serde_json::json!({"linked": 1e21}))
}

#[cfg(test)]
mod tests {
    #[test]
    fn vendored_port_links_with_js_number_parity() {
        // 1e21 is the JS exponent-spelling boundary — the exact divergence
        // class the port's serializer exists to pin.
        assert_eq!(super::lhc_port_linked(), r#"{"linked":1e+21}"#);
    }
}
