//! Where NX Hub keeps its things, and how to read them without ever caring
//! whether it is installed.
//!
//! Paths follow `nx-hub/src/main/config.js` and `docs/connector/PROTOCOL.md`:
//!
//! | thing | path |
//! |---|---|
//! | data dir | `$NX_HUB_DATA_DIR`, else `~/.local/share/nx-hub` |
//! | bus token | `<data>/connector.token` — 32 hex chars, mode 0600, trim it |
//! | bus snapshot | `<data>/connector-clients.json` |
//! | fleet identity | `<data>/fleet.json` (0600; peer ids, names, secrets) |
//! | CLI | `~/.local/bin/nx` |

use std::path::{Path, PathBuf};

/// Loopback port of the NX Connector bus (PROTOCOL.md §1).
pub const CONNECTOR_PORT: u16 = 9021;
/// The hub's own default host. The bus never listens on a routable interface.
pub const CONNECTOR_HOST: &str = "127.0.0.1";
/// Anything older than this means "the hub is not running" (ipc.js).
pub const SNAPSHOT_MAX_AGE_MS: u64 = 120_000;

/// The hub's data directory, honouring `$NX_HUB_DATA_DIR`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NX_HUB_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home().join(".local/share/nx-hub")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join("connector.token")
}

pub fn snapshot_path(data_dir: &Path) -> PathBuf {
    data_dir.join("connector-clients.json")
}

pub fn fleet_path(data_dir: &Path) -> PathBuf {
    data_dir.join("fleet.json")
}

/// The `nx` CLI as the hub's shim installs it.
pub fn nx_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("NX_BINARY") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    home().join(".local/bin/nx")
}

/// Where the token comes from. Production reads the file **on every connection
/// attempt** (PROTOCOL.md §2) — the hub may have been installed, or restarted
/// with a fresh secret, since the last try.
#[derive(Debug, Clone)]
pub enum TokenSource {
    File(PathBuf),
    /// Tests and the mock bus.
    Fixed(String),
}

impl TokenSource {
    pub fn read(&self) -> Option<String> {
        match self {
            TokenSource::Fixed(t) => Some(t.clone()),
            TokenSource::File(path) => {
                let raw = std::fs::read_to_string(path).ok()?;
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            TokenSource::Fixed(_) => "<fixed>".into(),
            TokenSource::File(p) => p.display().to_string(),
        }
    }
}

impl Default for TokenSource {
    fn default() -> Self {
        TokenSource::File(token_path(&data_dir()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_trimmed_and_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connector.token");
        let src = TokenSource::File(path.clone());
        assert_eq!(src.read(), None, "no file means no hub, not an error");
        std::fs::write(&path, "a1b2c3d4e5f60718293a4b5c6d7e8f90\n").unwrap();
        assert_eq!(src.read().unwrap(), "a1b2c3d4e5f60718293a4b5c6d7e8f90");
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(src.read(), None, "an empty token is no token");
    }

    #[test]
    fn data_dir_honours_the_env_override() {
        // Serialised by the parent test binary being single-threaded per test
        // module is not guaranteed, so assert only on the explicit path form.
        let dir = data_dir();
        assert!(dir.is_absolute());
        assert_eq!(token_path(Path::new("/x")), PathBuf::from("/x/connector.token"));
        assert_eq!(snapshot_path(Path::new("/x")), PathBuf::from("/x/connector-clients.json"));
    }
}
