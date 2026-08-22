//! **F57 — the single-instance lock, and the bug it exists to prevent.**
//!
//! A sibling project shipped a lock keyed on a fixed path. The consequence was
//! specific and infuriating: running the dev build took the *installed* copy's
//! lock, the dev instance concluded another copy was already running, exited
//! immediately, and helpfully raised the installed app's window instead. The
//! developer saw their change apparently do nothing.
//!
//! So the rule here is not "use a lock file", it is **the lock lives inside the
//! config dir** ([`crate::config::config_dir`]), which means
//! `NX_WISP_CONFIG_DIR` isolates it for free, along with everything else. A
//! test run, a second profile and the installed copy each hold their own lock
//! and cannot see each other's. There is no separate override to forget to set.
//!
//! # Mechanism
//!
//! `flock(2)` with `LOCK_EX | LOCK_NB`. The kernel releases it when the file
//! descriptor closes — including on `SIGKILL`, on a panic, and on an OOM kill —
//! so there is no stale-lock recovery path to get wrong. The pid written into
//! the file is *only* so the error message can name the process; it is never
//! consulted to decide whether the lock is held.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::config::LOCK_FILE;

/// Held for as long as this process is the running instance.
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

#[derive(Debug)]
pub enum LockError {
    /// Somebody else has it.
    Held { path: PathBuf, pid: Option<i32> },
    Io { path: PathBuf, source: std::io::Error },
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held { path, pid: Some(pid) } => write!(
                f,
                "She is already running as process {pid} in this config directory. \
                 Stop that copy first, or run with a different NX_WISP_CONFIG_DIR. \
                 (lock: {})",
                path.display()
            ),
            LockError::Held { path, pid: None } => write!(
                f,
                "She is already running in this config directory. Stop that copy first, \
                 or run with a different NX_WISP_CONFIG_DIR. (lock: {})",
                path.display()
            ),
            LockError::Io { path, source } => {
                write!(f, "Could not open the lock file {} — {source}", path.display())
            }
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join(LOCK_FILE)
}

/// Take the lock for `dir`, or say who has it.
pub fn acquire(dir: &Path) -> Result<InstanceLock, LockError> {
    let path = lock_path(dir);
    let io = |source| LockError::Io { path: path.clone(), source };

    std::fs::create_dir_all(dir).map_err(io)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(io)?;

    // SAFETY: `flock` takes a valid fd we own and an integer. It has no
    // memory-safety preconditions and the fd outlives the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => {
                Err(LockError::Held { pid: read_pid(&mut file), path })
            }
            _ => Err(LockError::Io { path, source: err }),
        };
    }

    // Ours. Stamp the pid so the *next* process can name us in its error.
    file.set_len(0).map_err(io)?;
    file.rewind().map_err(io)?;
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.flush();

    Ok(InstanceLock { file, path })
}

/// Is somebody else holding the lock for `dir`?
///
/// Used by `status` and `doctor` to say whether she is running. Racy by nature
/// — the answer is only ever "as of a moment ago" — so it is never used to
/// decide whether to start.
pub fn is_held(dir: &Path) -> bool {
    match acquire(dir) {
        Ok(l) => {
            drop(l);
            false
        }
        Err(LockError::Held { .. }) => true,
        // Cannot open it at all: not evidence that anybody is running.
        Err(LockError::Io { .. }) => false,
    }
}

/// The pid recorded in the lock file, if it is readable. Informational only.
pub fn holder_pid(dir: &Path) -> Option<i32> {
    let mut f = File::open(lock_path(dir)).ok()?;
    read_pid(&mut f)
}

fn read_pid(f: &mut File) -> Option<i32> {
    let mut s = String::new();
    f.rewind().ok()?;
    f.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

impl InstanceLock {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pid(&self) -> u32 {
        std::process::id()
    }

    /// Release explicitly. Dropping does the same thing; this exists so a
    /// shutdown path can be read top to bottom.
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // The kernel drops the flock when the fd closes, which happens as
        // `self.file` drops immediately after this. Unlinking is deliberately
        // *not* done: another process may already have the file open, and
        // removing it under them would let a third process create a fresh inode
        // and take a second, independent "exclusive" lock.
        let _ = self.file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;

    #[test]
    fn one_instance_per_config_dir() {
        let tmp = TempConfig::new();
        let first = acquire(tmp.path()).expect("the first copy takes it");
        let err = acquire(tmp.path()).expect_err("the second must not");
        match &err {
            LockError::Held { pid, .. } => {
                assert_eq!(*pid, Some(std::process::id() as i32));
            }
            other => panic!("{other}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("already running"), "{msg}");
        assert!(msg.contains("NX_WISP_CONFIG_DIR"), "the message must say what to do: {msg}");

        drop(first);
        acquire(tmp.path()).expect("released on drop");
    }

    /// The whole point of the module. A test run must never be able to take the
    /// installed copy's lock, nor it ours.
    #[test]
    fn a_different_config_dir_is_a_different_instance() {
        let tmp = TempConfig::new();
        let installed = tmp.path().join("installed");
        let dev = tmp.path().join("dev");
        let a = acquire(&installed).unwrap();
        let b = acquire(&dev).expect("an isolated run must never be blocked by another copy");
        assert_ne!(a.path(), b.path());
        assert!(is_held(&installed));
        assert!(is_held(&dev));
    }

    #[test]
    fn the_lock_is_inside_the_config_dir_so_the_override_covers_it() {
        let tmp = TempConfig::new();
        // Resolved from the environment, not passed in: this is the path the
        // running binary actually uses.
        let dir = crate::config::config_dir();
        assert_eq!(dir, tmp.path());
        let l = acquire(&dir).unwrap();
        assert!(l.path().starts_with(tmp.path()), "{}", l.path().display());
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        assert!(!l.path().starts_with(home.join(".config")));
    }

    #[test]
    fn is_held_is_false_for_a_free_directory_and_does_not_leave_the_lock_taken() {
        let tmp = TempConfig::new();
        assert!(!is_held(tmp.path()));
        assert!(!is_held(tmp.path()));
        acquire(tmp.path()).expect("probing must not have taken it");
    }

    #[test]
    fn the_recorded_pid_is_ours_and_survives_a_reacquire() {
        let tmp = TempConfig::new();
        {
            let _l = acquire(tmp.path()).unwrap();
            assert_eq!(holder_pid(tmp.path()), Some(std::process::id() as i32));
        }
        // Released, but the file and its pid stamp remain — which is why the
        // pid is never what decides whether the lock is held.
        assert_eq!(holder_pid(tmp.path()), Some(std::process::id() as i32));
        assert!(!is_held(tmp.path()));
    }
}
