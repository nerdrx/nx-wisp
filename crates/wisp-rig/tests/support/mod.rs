//! Shared test support.

use std::sync::OnceLock;

use tempfile::TempDir;

/// SPEC.md §4: **`NX_WISP_CONFIG_DIR` must be set to a temp dir by every test.**
///
/// `wisp-rig` is pure and never reads it — the rig has no state on disk — but
/// the rule has no exceptions, and a test binary that leaves it unset is one
/// refactor away from writing into the operator's real memory. Setting it here
/// costs nothing and means adding a filesystem read to this crate later cannot
/// quietly break the isolation.
///
/// The directory lives for the whole test binary, so a returned path stays
/// valid for the caller.
pub fn isolate_config_dir() -> &'static TempDir {
    static DIR: OnceLock<TempDir> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let d = tempfile::Builder::new()
            .prefix("nx-wisp-rig-test-")
            .tempdir()
            .expect("could not create a temp config dir");
        std::env::set_var("NX_WISP_CONFIG_DIR", d.path());
        d
    });
    // Re-assert on every call: another test in the same binary may have
    // changed it.
    std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
    dir
}

#[test]
fn the_config_dir_is_isolated() {
    let dir = isolate_config_dir();
    let set = std::env::var("NX_WISP_CONFIG_DIR").expect("NX_WISP_CONFIG_DIR was not set");
    assert_eq!(std::path::Path::new(&set), dir.path());
    assert!(dir.path().exists());
    // Never the operator's real config dir.
    assert!(!set.contains("/.config/nx-wisp"), "{set}");
}
