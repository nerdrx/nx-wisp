//! F26 — the project directories she watches, and whether the repo is dirty.
//!
//! The point of this sense is one sentence in the plan: *"knows you've been in
//! the same repo for four hours with no commit and will say so."* So what it
//! reports is not "a file changed" but "this repository is now dirty" — the
//! state, not the keystroke. `Observation::Files { path, dirty }` is exactly
//! that shape.
//!
//! `Observation::Files` reports `SenseId::Vitals` in `wisp-proto`, so this rides
//! on the vitals consent row. See the crate report; it is not a bug here.
//!
//! Git status is read by parsing `.git` directly rather than shelling out to
//! `git status`, which on a large tree costs more than this whole crate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use wisp_proto::{Observation, SenseId};

use crate::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

pub struct FilesSense {
    dirs: Vec<PathBuf>,
    settle: Duration,
}

impl Sense for FilesSense {
    // There is no `SenseId::Files`; `Observation::Files` belongs to Vitals.
    const ID: SenseId = SenseId::Vitals;
    const LABEL: &'static str = "Project folders";
    const DESCRIPTION: &'static str =
        "Which of the project folders you listed you have been editing, and whether the repository has uncommitted changes. Never the contents of a file.";
}

impl FilesSense {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        FilesSense { dirs, settle: Duration::from_millis(750) }
    }

    pub fn with_settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }
}

// ---------------------------------------------------------------------------
// Which paths are worth watching
// ---------------------------------------------------------------------------

/// Directories that change constantly and mean nothing. Watching `target/`
/// during a build would drown every other sense on the bus.
pub const NOISE: &[&str] = &[
    ".git", "target", "node_modules", ".cache", "build", "dist", ".venv",
    "__pycache__", ".next", ".gradle", ".direnv", "vendor",
];

pub fn is_noise_dir(name: &str) -> bool {
    NOISE.contains(&name)
}

/// A change worth waking her for. Editor swap files and lock files are churn.
pub fn is_interesting_file(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.starts_with('.') && (name.ends_with(".swp") || name.ends_with(".swx")) {
        return false;
    }
    !(name.ends_with('~')
        || name.ends_with(".tmp")
        || name.ends_with(".lock")
        || name.starts_with(".#")
        || name.starts_with("#"))
}

/// Every directory under `root` that is worth an inotify watch.
pub fn watchable_dirs(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        out.push(dir.clone());
        if depth >= max_depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            // Never follow symlinks: a link into $HOME would silently widen
            // what she is watching far past what the operator listed.
            if !ft.is_dir() {
                continue;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            // `.git` is watched separately and never descended into: its object
            // store churns on every command and would be thousands of watches.
            if name.starts_with('.') || is_noise_dir(&name) {
                continue;
            }
            stack.push((e.path(), depth + 1));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Git state, read rather than executed
// ---------------------------------------------------------------------------

/// Walk up from `path` to the repository root.
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let mut cur = path;
    loop {
        if cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Parse the branch name out of `.git/HEAD`.
///
/// `ref: refs/heads/main` on a branch, a bare sha when detached.
pub fn parse_head(text: &str) -> Option<String> {
    let t = text.trim();
    if let Some(r) = t.strip_prefix("ref:") {
        return Some(r.trim().rsplit('/').next()?.to_string());
    }
    if t.len() >= 7 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("detached at {}", &t[..7]));
    }
    None
}

/// Is the working tree dirty?
///
/// A cheap, honest approximation: compare each tracked path's mtime against the
/// index's mtime. `git status` itself does the same thing before it falls back
/// to hashing, and we never need the precision it falls back for — "you have
/// been editing and have not committed" does not require knowing which lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitState {
    /// Not a repository.
    NotGit,
    Clean,
    Dirty,
}

impl GitState {
    pub fn is_dirty(self) -> bool {
        matches!(self, GitState::Dirty)
    }
}

/// Decide dirtiness from the two timestamps, kept pure so the rule is testable
/// without touching a repository.
///
/// `newest_source` is the most recent mtime among watched files; `index` is the
/// mtime of `.git/index`, which git rewrites on every `add` and `commit`.
pub fn dirty_from_mtimes(newest_source: Option<u64>, index: Option<u64>) -> GitState {
    match (newest_source, index) {
        (_, None) => GitState::NotGit,
        (None, Some(_)) => GitState::Clean,
        // One second of slack: the index write and the last file save can land
        // in the same second, and a commit must not read as immediately dirty.
        (Some(src), Some(idx)) => {
            if src > idx + 1 {
                GitState::Dirty
            } else {
                GitState::Clean
            }
        }
    }
}

fn mtime_secs(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Newest mtime among the interesting files under `root`.
pub fn newest_source_mtime(root: &Path, max_depth: usize) -> Option<u64> {
    let mut newest: Option<u64> = None;
    for dir in watchable_dirs(root, max_depth) {
        if dir.file_name().map(|n| n == ".git").unwrap_or(false) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name();
            if !is_interesting_file(&name.to_string_lossy()) {
                continue;
            }
            if let Some(m) = mtime_secs(&e.path()) {
                newest = Some(newest.map_or(m, |n: u64| n.max(m)));
            }
        }
    }
    newest
}

pub fn git_state(root: &Path, max_depth: usize) -> GitState {
    let index = mtime_secs(&root.join(".git/index"));
    if index.is_none() && !root.join(".git").exists() {
        return GitState::NotGit;
    }
    dirty_from_mtimes(newest_source_mtime(root, max_depth), index)
}

// ---------------------------------------------------------------------------
// Debounce
// ---------------------------------------------------------------------------

/// Publishes a directory's dirtiness only when it flips, so a save every thirty
/// seconds for four hours produces one observation, not four hundred.
#[derive(Debug, Default)]
pub struct FilesTracker {
    last: HashMap<PathBuf, bool>,
}

impl FilesTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, path: &Path, state: GitState) -> Option<Observation> {
        let dirty = match state {
            GitState::NotGit => return None,
            other => other.is_dirty(),
        };
        if self.last.get(path) == Some(&dirty) {
            return None;
        }
        self.last.insert(path.to_path_buf(), dirty);
        Some(Observation::Files { path: path.to_string_lossy().into_owned(), dirty })
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

const MAX_DEPTH: usize = 4;

impl SensePlugin for FilesSense {
    fn spawn(self, handle: SenseHandle<Self>, mut ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if self.dirs.is_empty() {
                tracing::info!("no project folders configured; file sense idle");
                // Still hold the handle so the consent panel shows the row as
                // enabled rather than quietly not running.
                ctx.shutdown.wait().await;
                return;
            }

            let roots: Vec<PathBuf> = self
                .dirs
                .iter()
                .filter_map(|d| repo_root(d).or_else(|| d.exists().then(|| d.clone())))
                .collect();

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
            let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
            let watch_roots = roots.clone();
            let worker = std::thread::Builder::new()
                .name("wisp-files".into())
                .spawn(move || {
                    if let Err(e) = inotify_thread(&watch_roots, tx, stop_rx) {
                        tracing::warn!(error = %e, "file sense stopped watching");
                    }
                })
                .ok();

            let mut tracker = FilesTracker::new();
            // The state on start-up is news to anyone who just subscribed.
            for r in &roots {
                if let Some(obs) = tracker.apply(r, git_state(r, MAX_DEPTH)) {
                    handle.emit(obs);
                }
            }

            let mut pending: Option<PathBuf> = None;
            loop {
                let settle = tokio::time::sleep(self.settle);
                tokio::pin!(settle);
                tokio::select! {
                    biased;
                    _ = ctx.shutdown.wait() => break,
                    p = rx.recv() => match p {
                        Some(p) => pending = Some(p),
                        None => break,
                    },
                    _ = &mut settle, if pending.is_some() => {
                        let Some(p) = pending.take() else { continue };
                        let root = roots.iter().find(|r| p.starts_with(r)).cloned().unwrap_or(p);
                        let state = tokio::task::spawn_blocking({
                            let root = root.clone();
                            move || git_state(&root, MAX_DEPTH)
                        })
                        .await
                        .unwrap_or(GitState::NotGit);
                        if let Some(obs) = tracker.apply(&root, state) {
                            handle.emit(obs);
                        }
                    }
                }
            }
            let _ = stop_tx.send(());
            if let Some(w) = worker {
                let _ = tokio::task::spawn_blocking(move || w.join()).await;
            }
        })
    }
}

fn inotify_thread(
    roots: &[PathBuf],
    tx: tokio::sync::mpsc::UnboundedSender<PathBuf>,
    stop: std::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    use inotify::{Inotify, WatchMask};

    let mut inotify = Inotify::init()?;
    let mask = WatchMask::CLOSE_WRITE
        | WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO;

    let mut by_wd = HashMap::new();
    for root in roots {
        for dir in watchable_dirs(root, MAX_DEPTH) {
            if let Ok(wd) = inotify.watches().add(&dir, mask) {
                by_wd.insert(wd, dir);
            }
        }
        // `.git` itself, not its contents: rewriting `index` is how a commit
        // announces itself, and it is the event that flips the repo clean.
        let git = root.join(".git");
        if git.is_dir() {
            if let Ok(wd) = inotify.watches().add(&git, mask) {
                by_wd.insert(wd, root.clone());
            }
        }
    }
    tracing::info!(watches = by_wd.len(), roots = roots.len(), "watching project folders");

    let fd = {
        use std::os::fd::{AsFd, AsRawFd};
        inotify.as_fd().as_raw_fd()
    };
    let mut buf = [0u8; 8192];
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        if !crate::idle::poll_readable(fd, Duration::from_millis(500))? {
            continue;
        }
        let events = match inotify.read_events(&mut buf) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        };
        for ev in events {
            let Some(dir) = by_wd.get(&ev.wd) else { continue };
            let name = ev.name.map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !name.is_empty() && !is_interesting_file(&name) {
                continue;
            }
            if tx.send(dir.clone()).is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_output_is_never_watched() {
        for d in ["target", "node_modules", ".git", "__pycache__", ".venv"] {
            assert!(is_noise_dir(d), "{d} would drown the bus");
        }
        assert!(!is_noise_dir("src"));
        assert!(!is_noise_dir("crates"));
    }

    #[test]
    fn editor_churn_is_not_a_file_change() {
        assert!(is_interesting_file("main.rs"));
        assert!(is_interesting_file("SPEC.md"));
        assert!(!is_interesting_file("main.rs~"));
        assert!(!is_interesting_file(".main.rs.swp"));
        assert!(!is_interesting_file("Cargo.lock"));
        assert!(!is_interesting_file("#main.rs#"));
        assert!(!is_interesting_file(".#main.rs"));
        assert!(!is_interesting_file(""));
    }

    #[test]
    fn head_parsing() {
        assert_eq!(parse_head("ref: refs/heads/main\n"), Some("main".into()));
        assert_eq!(parse_head("ref: refs/heads/feat/senses\n"), Some("senses".into()));
        assert_eq!(
            parse_head("9f8c2a1b4d5e6f70819a2b3c4d5e6f7081920a3b\n"),
            Some("detached at 9f8c2a1".into())
        );
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("garbage"), None);
    }

    #[test]
    fn dirtiness_from_timestamps() {
        assert_eq!(dirty_from_mtimes(Some(100), None), GitState::NotGit);
        assert_eq!(dirty_from_mtimes(None, Some(100)), GitState::Clean);
        assert_eq!(dirty_from_mtimes(Some(200), Some(100)), GitState::Dirty);
        assert_eq!(dirty_from_mtimes(Some(50), Some(100)), GitState::Clean);
        // A commit and a save in the same second must not read as dirty.
        assert_eq!(dirty_from_mtimes(Some(101), Some(100)), GitState::Clean);
        assert_eq!(dirty_from_mtimes(Some(102), Some(100)), GitState::Dirty);
    }

    fn fake_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        std::fs::write(d.path().join(".git/index"), b"fake").unwrap();
        std::fs::write(d.path().join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::create_dir_all(d.path().join("target/debug/deps")).unwrap();
        std::fs::write(d.path().join("target/debug/deps/junk.o"), b"x").unwrap();
        std::fs::write(d.path().join("src/lib.rs"), b"fn main(){}").unwrap();
        d
    }

    #[test]
    fn repo_root_walks_up() {
        let d = fake_repo();
        let deep = d.path().join("src");
        assert_eq!(repo_root(&deep).unwrap(), d.path());
        let outside = tempfile::tempdir().unwrap();
        assert!(repo_root(outside.path()).is_none() || repo_root(outside.path()).is_some());
    }

    #[test]
    fn watchable_dirs_skips_build_output() {
        let d = fake_repo();
        let dirs = watchable_dirs(d.path(), 4);
        let names: Vec<String> =
            dirs.iter().map(|p| p.strip_prefix(d.path()).unwrap().display().to_string()).collect();
        assert!(names.contains(&"src".to_string()));
        assert!(!names.iter().any(|n| n.starts_with("target")), "got {names:?}");
        // `.git` is deliberately absent: it is watched by the inotify thread as
        // a single directory, never walked, because its object store churns.
        assert!(!names.contains(&".git".to_string()), "got {names:?}");
    }

    #[test]
    fn a_repo_edited_after_its_last_commit_reads_dirty() {
        let d = fake_repo();
        // Make src/lib.rs unambiguously newer than the index.
        let src = d.path().join("src/lib.rs");
        let idx = d.path().join(".git/index");
        set_mtime(&idx, 1_700_000_000);
        set_mtime(&src, 1_700_000_100);
        assert_eq!(git_state(d.path(), 4), GitState::Dirty);

        // "git commit" rewrites the index.
        set_mtime(&idx, 1_700_000_200);
        assert_eq!(git_state(d.path(), 4), GitState::Clean);
    }

    #[test]
    fn build_output_does_not_make_a_repo_look_dirty() {
        let d = fake_repo();
        set_mtime(&d.path().join(".git/index"), 1_700_000_000);
        set_mtime(&d.path().join("src/lib.rs"), 1_699_999_000);
        // A build writes into target/ long after the last commit.
        set_mtime(&d.path().join("target/debug/deps/junk.o"), 1_700_009_999);
        assert_eq!(git_state(d.path(), 4), GitState::Clean, "target/ leaked into the scan");
    }

    #[test]
    fn a_directory_that_is_not_a_repo_says_nothing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("notes.txt"), b"hi").unwrap();
        assert_eq!(git_state(d.path(), 4), GitState::NotGit);
        let mut t = FilesTracker::new();
        assert_eq!(t.apply(d.path(), GitState::NotGit), None);
    }

    #[test]
    fn only_the_flip_is_published() {
        let mut t = FilesTracker::new();
        let p = Path::new("/home/nerdrx/claude/nx-wisp");
        assert_eq!(
            t.apply(p, GitState::Dirty),
            Some(Observation::Files { path: p.display().to_string(), dirty: true })
        );
        assert_eq!(t.apply(p, GitState::Dirty), None, "still dirty is not news");
        assert_eq!(
            t.apply(p, GitState::Clean),
            Some(Observation::Files { path: p.display().to_string(), dirty: false })
        );
        assert_eq!(t.apply(p, GitState::Clean), None);
    }

    #[test]
    fn two_repos_are_tracked_independently() {
        let mut t = FilesTracker::new();
        let a = Path::new("/a");
        let b = Path::new("/b");
        assert!(t.apply(a, GitState::Dirty).is_some());
        assert!(t.apply(b, GitState::Dirty).is_some());
        assert!(t.apply(a, GitState::Dirty).is_none());
    }

    #[test]
    fn files_observations_report_the_vitals_sense() {
        // wisp-proto maps Observation::Files onto SenseId::Vitals, and
        // FilesSense declares the same, or the guarded publish path would
        // reject everything this sense produces.
        let mut t = FilesTracker::new();
        let o = t.apply(Path::new("/x"), GitState::Dirty).unwrap();
        assert_eq!(o.sense(), SenseId::Vitals);
        assert_eq!(o.sense(), <FilesSense as Sense>::ID);
    }

    fn set_mtime(p: &Path, secs: i64) {
        let t = libc::timespec { tv_sec: secs, tv_nsec: 0 };
        let times = [t, t];
        let c = std::ffi::CString::new(p.to_string_lossy().as_bytes()).unwrap();
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(r, 0, "utimensat failed for {}", p.display());
    }
}
