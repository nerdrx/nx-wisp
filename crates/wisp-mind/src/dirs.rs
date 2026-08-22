//! Where she keeps her head.
//!
//! SPEC §4: **`NX_WISP_CONFIG_DIR` must be set to a temp dir by every test.**
//! The resolution below is byte-for-byte the same as `wisp::config::config_dir`
//! and `wisp_senses::consent::config_dir`, deliberately — three crates that
//! disagreed about where the operator's state lives would be a data-loss bug,
//! not a style difference. `wisp-mind` may not depend on either of them
//! (SPEC §2), so this is a copy that exists to be kept identical.
//!
//! A 18 GiB GGUF does not belong in a directory people back up as dotfiles, so
//! models go under [`data_dir`], and an isolated config dir drags the data dir
//! along with it so a test can never be pointed at the operator's real models.

use std::path::PathBuf;

pub const APP_ID: &str = "nx-wisp";

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `$NX_WISP_CONFIG_DIR`, else `$XDG_CONFIG_HOME/nx-wisp`, else
/// `~/.config/nx-wisp`.
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NX_WISP_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join(APP_ID);
        }
    }
    home().join(".config").join(APP_ID)
}

/// Where models and other large generated things go.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NX_WISP_DATA_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    // An isolated config dir must not leave models pointing at the real one.
    if std::env::var_os("NX_WISP_CONFIG_DIR").is_some() {
        return config_dir().join("data");
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join(APP_ID);
        }
    }
    home().join(".local").join("share").join(APP_ID)
}

/// `<data>/models` — GGUFs, and nothing else.
pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

/// `<data>/mind` — the memory database and the tool-consent state.
pub fn mind_dir() -> PathBuf {
    data_dir().join("mind")
}

/// The memory database. One file, one operator (SPEC §0.5).
pub fn memory_db() -> PathBuf {
    mind_dir().join("memory.sqlite3")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §4 again: this test sets the variable it is testing, which is the
    /// only honest way to test it.
    #[test]
    fn an_isolated_config_dir_drags_the_data_dir_with_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Safety: single-threaded assertion about process env. The rest of the
        // suite uses `crate::testing::Isolated`, which serialises this.
        let _guard = crate::testing::Isolated::at(tmp.path());
        assert_eq!(config_dir(), tmp.path());
        assert_eq!(data_dir(), tmp.path().join("data"));
        assert!(models_dir().starts_with(tmp.path()));
        assert!(memory_db().starts_with(tmp.path()));
    }
}
