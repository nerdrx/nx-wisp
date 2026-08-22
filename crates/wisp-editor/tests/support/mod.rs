//! Shared setup. Every test file calls [`isolate`] before it touches state.

#![allow(dead_code)]

use std::path::PathBuf;

use wisp_editor::editor::Editor;
use wisp_rig::skin::doc::SkinDoc;

/// SPEC §4: no test may share state with the operator's installed copy.
/// Setting the config dir to a per-process temp directory is not optional —
/// this suite has bitten this project before (NX Orbit, 2026-08-20).
pub fn isolate() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nx-wisp-editor-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp dir");
    if std::env::var_os("NX_WISP_CONFIG_DIR").is_none() {
        std::env::set_var("NX_WISP_CONFIG_DIR", &dir);
    }
    dir
}

/// The shipped skin as a document — the primary fixture, and the one that
/// matters: if the editor cannot hold *her*, it cannot hold anything.
pub fn shipped_doc() -> SkinDoc {
    toml::from_str(wisp_rig::skin::WISP_SKIN_TOML).expect("the shipped skin parses")
}

pub fn shipped_source() -> &'static str {
    wisp_rig::skin::WISP_SKIN_TOML
}

/// An editor over the shipped skin.
pub fn shipped_editor() -> Editor {
    isolate();
    Editor::default_skin().expect("the shipped skin opens")
}

/// Serialise a document the way a save would, with no comments — the canonical
/// form the round-trip tests compare.
pub fn canonical(doc: &SkinDoc) -> String {
    wisp_editor::save::to_toml(doc).expect("a document serialises")
}
