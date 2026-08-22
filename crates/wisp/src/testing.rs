//! Test scaffolding. SPEC §4: **every** test that touches config sets
//! `NX_WISP_CONFIG_DIR` to a temp dir, with no exceptions. In this crate that
//! matters more than anywhere else, because this crate is the one that decides
//! where the lock, the flight recorder and the state file go — and a test that
//! took the real lock would raise the installed copy's window instead of
//! starting (the exact bug a sibling project shipped).
//!
//! The mutex is not optional. Tests run on threads of one process, so
//! `set_var` is process-wide; without serialising, one test's temp dir leaks
//! into another's `config_dir()`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A temp directory installed as `NX_WISP_CONFIG_DIR` for as long as it lives.
pub struct TempConfig {
    dir: tempfile::TempDir,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _guard: MutexGuard<'static, ()>,
}

/// Everything this crate resolves out of the environment. All of it is
/// redirected together, so a test can never write into the operator's real
/// state through a path we forgot about.
const REDIRECTED: [&str; 4] =
    ["NX_WISP_CONFIG_DIR", "NX_WISP_DATA_DIR", "NX_WISP_INSTALL_ROOT", "NX_WISP_SCRIPT_DIR"];

impl TempConfig {
    pub fn new() -> Self {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("temp dir");
        let mut previous = Vec::new();
        for key in REDIRECTED {
            previous.push((key, std::env::var_os(key)));
        }
        std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
        std::env::set_var("NX_WISP_DATA_DIR", dir.path().join("data"));
        std::env::set_var("NX_WISP_INSTALL_ROOT", dir.path().join("install"));
        std::env::set_var("NX_WISP_SCRIPT_DIR", dir.path().join("run"));
        TempConfig { dir, previous, _guard: guard }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// The install root this temp config redirects to.
    pub fn install_root(&self) -> PathBuf {
        self.dir.path().join("install")
    }
}

impl Default for TempConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        for (key, value) in std::mem::take(&mut self.previous) {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_redirects_everything_and_puts_it_all_back() {
        let mine = {
            let t = TempConfig::new();
            assert_eq!(crate::config::config_dir(), t.path());
            assert_eq!(crate::install::install_root(), t.install_root());
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            assert!(
                !crate::config::config_dir().starts_with(home.join(".config")),
                "a test must never resolve to the operator's real config dir"
            );
            t.to_path_buf()
        };
        assert_ne!(
            std::env::var_os("NX_WISP_CONFIG_DIR").map(PathBuf::from),
            Some(mine),
            "TempConfig leaked its directory past its own lifetime"
        );
    }
}
