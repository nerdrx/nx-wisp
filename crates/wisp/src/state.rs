//! What the running instance publishes about itself, so `wisp status` can
//! answer without an IPC channel.
//!
//! The CLI and the GUI drive the same modules (F53), but `status` has to work
//! from a *second* process, and the alternatives to a state file are all worse
//! for a companion that must cost nothing at T3: a D-Bus name is a service to
//! keep alive, and a socket is a reader to poll. A small file written on tier
//! change (and at most every couple of seconds otherwise) costs one atomic
//! rename and nothing at all while she is idle.
//!
//! It is a *cache of facts already in the flight recorder*, never a second
//! source of truth. If it is missing or stale, `status` falls back to the
//! recorder and to a live probe, and says which it used.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use wisp_proto::{Cost, SenseId, Tier};

use crate::config::STATE_FILE;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Wall-clock epoch ms when this was written. Only used to decide whether
    /// the file is stale.
    pub written_ms: u64,
    /// The run that wrote it — the same id the flight recorder stamps.
    pub session: u64,
    pub pid: u32,
    pub version: String,

    pub tier: Tier,
    /// `"T3 because WiVRn is streaming"`, straight from the governor.
    pub because: String,
    /// The cost meter's sentence (F66).
    pub headline: String,
    pub estimated: Cost,
    pub measured_rss_mib: u32,
    pub measured_cpu_centi_pct: u32,
    pub dgpu_vram_mib: u64,
    pub dgpu_untouched: bool,
    pub by_subsystem: Vec<(String, Cost)>,

    /// Senses that are enabled *and* actually running right now.
    pub senses_live: Vec<SenseId>,
    /// Invasive senses currently live — the visible tell of SPEC §0.3, in text.
    pub invasive_live: Vec<SenseId>,

    pub chattiness: String,
    pub silenced: bool,
    pub pinned: Option<Tier>,
    /// Held utterances waiting for a moment.
    pub waiting: usize,
    /// The last thing she said, if anything.
    pub last_said: Option<String>,
    pub mock: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            written_ms: 0,
            session: 0,
            pid: 0,
            version: crate::VERSION.to_string(),
            tier: Tier::Full,
            because: String::new(),
            headline: String::new(),
            estimated: Cost::FREE,
            measured_rss_mib: 0,
            measured_cpu_centi_pct: 0,
            dgpu_vram_mib: 0,
            dgpu_untouched: true,
            by_subsystem: Vec::new(),
            senses_live: Vec::new(),
            invasive_live: Vec::new(),
            chattiness: "occasional".to_string(),
            silenced: false,
            pinned: None,
            waiting: 0,
            last_said: None,
            mock: false,
        }
    }
}

/// Anything older than this is not describing the machine any more.
pub const STALE_AFTER_MS: u64 = 30_000;

impl State {
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.written_ms)
    }

    pub fn is_stale(&self, now_ms: u64) -> bool {
        self.age_ms(now_ms) > STALE_AFTER_MS
    }
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// Atomic write-then-rename, so a reader never sees half a state.
pub fn save(dir: &Path, s: &State) -> std::io::Result<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let p = path(dir);
    let tmp = dir.join(format!(".{STATE_FILE}.{}.tmp", std::process::id()));
    let mut json = serde_json::to_vec_pretty(s)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    json.push(b'\n');
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
    }
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

/// `None` when she has never run in this config dir, or the file is unreadable.
/// A corrupt state file is not worth reporting: it is a cache.
pub fn load(dir: &Path) -> Option<State> {
    let bytes = std::fs::read(path(dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Best effort tidy-up on a clean exit, so `status` says "not running" rather
/// than showing a state that is merely recent.
pub fn clear(dir: &Path) {
    let _ = std::fs::remove_file(path(dir));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;

    #[test]
    fn round_trips_and_leaves_no_temporary() {
        let tmp = TempConfig::new();
        let mut s = State { tier: Tier::Lobotomised, ..State::default() };
        s.because = "T3 because a game is running".into();
        s.senses_live = vec![SenseId::Idle, SenseId::Vitals];
        save(tmp.path(), &s).unwrap();
        assert_eq!(load(tmp.path()).unwrap(), s);
        let strays: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }

    #[test]
    fn a_missing_or_corrupt_state_is_simply_absent() {
        let tmp = TempConfig::new();
        assert!(load(tmp.path()).is_none());
        std::fs::write(path(tmp.path()), b"not json").unwrap();
        assert!(load(tmp.path()).is_none());
    }

    #[test]
    fn staleness_is_measured_not_assumed() {
        let s = State { written_ms: 1_000_000, ..State::default() };
        assert!(!s.is_stale(1_000_000 + STALE_AFTER_MS));
        assert!(s.is_stale(1_000_000 + STALE_AFTER_MS + 1));
        assert_eq!(s.age_ms(1_005_000), 5_000);
    }

    #[test]
    fn clearing_removes_it() {
        let tmp = TempConfig::new();
        save(tmp.path(), &State::default()).unwrap();
        clear(tmp.path());
        assert!(load(tmp.path()).is_none());
    }
}
