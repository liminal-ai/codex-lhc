//! Feature gate and LHC storage root for Codex capture.
//!
//! LHC runs by default in the product fork. The `lhc_capture = false` config
//! value is its single kill switch. Tests may override the storage root via
//! `CODEX_LHC_ROOT`.

use std::path::PathBuf;

/// Root directory for LHC registry + thread SQLite files.
///
/// Override with `CODEX_LHC_ROOT` (tests). Default: `~/.codex/lhc`.
pub fn lhc_root() -> PathBuf {
    if let Ok(root) = std::env::var("CODEX_LHC_ROOT") {
        let trimmed = root.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".codex").join("lhc")
}

/// Process-wide lock for tests that mutate `CODEX_LHC*` env vars.
#[cfg(any(test, feature = "test-util"))]
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_honors_env_override() {
        let _g = env_lock();
        let prev = std::env::var_os("CODEX_LHC_ROOT");
        // SAFETY: test process; env restored below.
        unsafe { std::env::set_var("CODEX_LHC_ROOT", "/tmp/lhc-test-root") };
        assert_eq!(lhc_root(), PathBuf::from("/tmp/lhc-test-root"));
        match prev {
            Some(v) => unsafe { std::env::set_var("CODEX_LHC_ROOT", v) },
            None => unsafe { std::env::remove_var("CODEX_LHC_ROOT") },
        }
    }
}
