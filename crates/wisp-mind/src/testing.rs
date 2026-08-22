//! SPEC §4 in one type.
//!
//! > **`NX_WISP_CONFIG_DIR` must be set to a temp dir by every test.** The dev
//! > build and the installed copy otherwise share state, and test fixtures then
//! > write into the operator's real memory.
//!
//! This module is public rather than `cfg(test)` for the same reason
//! `wisp_gov::fakes` is: the integration tests in `tests/` are a separate crate
//! and cannot reach into `#[cfg(test)]`. It deliberately does **not** depend on
//! `tempfile` — the caller owns the directory, so there is no way for this to
//! quietly create one somewhere it should not.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Every variable that can move her state somewhere else. All of them are
/// cleared, not just the one being set — a leftover `NX_WISP_DATA_DIR` from
/// another test would otherwise put a fixture's models in the operator's
/// directory, which is exactly the accident SPEC §4 is about.
const VARS: [&str; 2] = ["NX_WISP_CONFIG_DIR", "NX_WISP_DATA_DIR"];

fn lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Points `NX_WISP_CONFIG_DIR` at a directory for as long as it lives, and puts
/// the environment back afterwards.
///
/// Holds a process-wide lock, so tests that need isolation are serialised
/// against each other within a test binary. That is a feature: the environment
/// is process-global and pretending otherwise is how suites get flaky.
pub struct Isolated {
    dir: PathBuf,
    saved: Vec<(&'static str, Option<OsString>)>,
    _guard: MutexGuard<'static, ()>,
}

impl Isolated {
    /// Isolate at `dir`, which the caller owns (usually a `tempfile::TempDir`).
    pub fn at(dir: impl AsRef<Path>) -> Self {
        let guard = lock();
        let saved = VARS
            .iter()
            .map(|v| (*v, std::env::var_os(v)))
            .collect::<Vec<_>>();
        for v in VARS {
            std::env::remove_var(v);
        }
        let dir = dir.as_ref().to_path_buf();
        std::env::set_var("NX_WISP_CONFIG_DIR", &dir);
        debug_assert!(std::env::var_os("NX_WISP_CONFIG_DIR").is_some());
        Isolated {
            dir,
            saved,
            _guard: guard,
        }
    }

    pub fn config_dir(&self) -> &Path {
        &self.dir
    }
    pub fn data_dir(&self) -> PathBuf {
        crate::dirs::data_dir()
    }
    /// Create and return `<data>/models`, ready to be written into.
    pub fn models_dir(&self) -> PathBuf {
        let d = crate::dirs::models_dir();
        std::fs::create_dir_all(&d).expect("models dir");
        d
    }
    /// Create and return `<data>/mind`.
    pub fn mind_dir(&self) -> PathBuf {
        let d = crate::dirs::mind_dir();
        std::fs::create_dir_all(&d).expect("mind dir");
        d
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

/// A fixed monotonic clock for tests that need to talk about time passing
/// without any actual time passing (SPEC §3.2: never wall-clock for ordering).
#[derive(Debug, Clone, Default)]
pub struct Clock {
    now: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Clock {
    pub fn new(start_ms: u64) -> Self {
        Clock {
            now: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(start_ms)),
        }
    }
    pub fn now(&self) -> wisp_proto::Millis {
        self.now.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn advance(&self, ms: u64) -> wisp_proto::Millis {
        self.now
            .fetch_add(ms, std::sync::atomic::Ordering::Relaxed)
            + ms
    }
}

/// Milliseconds in a day, for the memory-decay tests that have to talk in
/// weeks without waiting for one.
pub const DAY_MS: i64 = 86_400_000;
