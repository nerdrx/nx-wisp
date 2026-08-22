//! Shared test scaffolding.
//!
//! SPEC §4: **`NX_WISP_CONFIG_DIR` must be set to a temp dir by every test.**
//! The dev build and the installed copy otherwise share state and fixtures
//! write into the operator's real memory. Every test file in this crate calls
//! [`isolate`] first, and no test in this crate writes anywhere except under the
//! directory it is handed.

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// A directory that deletes itself. No `tempfile` dependency: this crate ships
/// four dependencies and is not adding a fifth for four lines of code.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "wisp-gov-test-{}-{}-{}-{}",
            tag,
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
    /// Create a file, making its parents.
    pub fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create parents");
        }
        std::fs::write(&p, contents).expect("write fixture");
        p
    }
    pub fn mkdir(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(&p).expect("create dir");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Point `NX_WISP_CONFIG_DIR` at a scratch directory for the whole test binary.
/// Returns the guard; hold it for the life of the test.
pub fn isolate(tag: &str) -> TempDir {
    let dir = TempDir::new(tag);
    std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
    dir
}
