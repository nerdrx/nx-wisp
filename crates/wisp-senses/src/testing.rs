//! Test scaffolding. SPEC §4: **every** test that touches config or state sets
//! `NX_WISP_CONFIG_DIR` to a temp dir. The dev build and the installed copy
//! otherwise share state and fixtures write into the operator's real memory —
//! this bit the NX Orbit suite on 2026-08-20. No exceptions.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialises tests that mutate the process environment, so a parallel test can
/// never observe another test's `NX_WISP_CONFIG_DIR`.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A temp directory installed as `NX_WISP_CONFIG_DIR` for as long as it lives.
/// Restores the previous value on drop.
pub struct TempConfig {
    dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
    _guard: MutexGuard<'static, ()>,
}

impl TempConfig {
    pub fn new() -> Self {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("temp dir");
        let previous = std::env::var_os("NX_WISP_CONFIG_DIR");
        std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
        TempConfig { dir, previous, _guard: guard }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }
}

impl Default for TempConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(v) => std::env::set_var("NX_WISP_CONFIG_DIR", v),
            None => std::env::remove_var("NX_WISP_CONFIG_DIR"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_config_redirects_and_restores() {
        let mine = {
            let t = TempConfig::new();
            assert_eq!(crate::consent::config_dir(), t.path());
            assert_ne!(
                crate::consent::config_dir(),
                dirs_like_home(),
                "config dir must never be the operator's real one during tests"
            );
            t.to_path_buf()
        };
        // Reading the outer value directly would race another test that has
        // since taken the env lock; asserting our own path is gone does not.
        assert_ne!(
            std::env::var_os("NX_WISP_CONFIG_DIR").map(PathBuf::from),
            Some(mine),
            "TempConfig leaked its directory past its own lifetime"
        );
    }

    fn dirs_like_home() -> PathBuf {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".config").join("nx-wisp")
    }
}
