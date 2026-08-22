//! F33's first half — turning the operator's music down while she talks, and
//! **always** putting it back.
//!
//! The governing sentence of this module, and the reason half of it is about
//! crash recovery rather than about volume: *a companion that permanently ducks
//! your music is worse than one that never speaks.* Every design decision below
//! falls out of taking that literally. A duck is a debt. The journal is how the
//! debt survives a `SIGKILL`, and [`Ducker::recover`] is how it gets paid.
//!
//! ```text
//!   duck(now) ─┬─ 0→1 ─▶ list streams ─▶ write journal ─▶ ramp volumes down
//!              └─ 1→2 ─▶ nothing (one duck, not two)
//!
//!   release(now) ─┬─ 2→1 ─▶ nothing
//!                 └─ 1→0 ─▶ list streams ─▶ restore ─▶ delete journal
//!
//!   ...or she is killed here, and the next start does:
//!   recover(mixer) ─▶ read journal ─▶ restore ─▶ delete journal
//! ```
//!
//! ## Why the ref count
//!
//! Two clauses of one sentence, or a spoken line landing while a notification
//! chime is already playing, are *one* duck. Only 0→1 touches a volume and only
//! 1→0 restores. This is the same shape as the live-handle count in
//! `wisp_senses::consent::ConsentLedger` — there a count rather than a flag is
//! what keeps the §0.3 tell exact when two senses share a `SenseId`; here it is
//! what keeps the music from being restored halfway through a sentence.
//!
//! ## Why a `StreamKey` is not an index
//!
//! `pactl` reports a sink-input `index`, and on real PulseAudio those indices
//! are small and **recycled**. Under `pipewire-pulse` the index happens to be
//! the node's `object.serial`, which is a monotonically increasing 64-bit
//! counter that is never reused — but the pulse *protocol* makes no such
//! promise, and a restore that lands on a stream that merely inherited a number
//! is the precise failure this module exists to prevent: her sentence ends and
//! some unrelated app is suddenly at 34%, forever, with nothing on disk saying
//! why.
//!
//! So [`StreamKey`] is `object.serial` plus `application.name`. The serial is
//! the identity; the app name is a cross-check that costs nothing and catches
//! the case where a backend has no serial to give and falls back to the index.
//! A key only matches if **both** halves match, and a stream that does not match
//! is treated as gone rather than as a target.
//!
//! ## The operator always wins
//!
//! If a volume changed while it was ducked, the operator reached for the knob,
//! and their number is now the truth. Restore compares the live volume against
//! the value *we last set* (not the value we originally read) and leaves
//! anything that moved alone. [`OPERATOR_EPSILON`] is the tolerance, sized for
//! the round trip through `pactl`'s integer percentages — a percent of rounding
//! is us, five percent is a human.
//!
//! ## What is deliberately *not* here
//!
//! No clock and no thread. The fade is a ramp the caller advances with
//! [`Ducker::tick`], because a background thread that owns volumes is a
//! background thread that can outlive the decision to stop ducking. And the
//! ramp only runs *downward*: [`Ducker::release`] restores in one step. A fade
//! that a crash interrupts on the way down leaves the music quiet and the
//! journal already written, which recovers; a fade interrupted on the way *up*
//! leaves the music quiet with the journal already deleted, which does not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audio::from_db;
use crate::{data_dir, Millis, Result, VoiceError};

/// The `application.name` her own playback stream sets, and therefore the one
/// name that ships in [`DuckConfig::exempt`]. Ducking herself would make her
/// quieter every time she spoke over herself.
pub const OWN_APP: &str = "NX Wisp";

/// PulseAudio's `PA_VOLUME_NORM`. A channel's `value` is relative to this, so
/// `65536` is 100% and values above it are software boost.
pub const VOLUME_NORM: f64 = 65536.0;

/// How far a live volume may sit from the value we set and still count as
/// "untouched since we set it".
///
/// Two percentage points. `pactl set-sink-input-volume … 63%` quantises to
/// whole percent, so a clean round trip is off by at most half a point; a
/// deliberate nudge of a slider or a media key is five or more. Anything in
/// between is ambiguous and we resolve it in the operator's favour by *not*
/// stomping them — the cost of guessing wrong that way is one stream left where
/// they put it, and the cost of guessing wrong the other way is the bug in the
/// first paragraph.
pub const OPERATOR_EPSILON: f32 = 0.02;

/// Smallest volume change worth spending a `pactl` invocation on mid-ramp.
const RAMP_MIN_STEP: f32 = 0.02;

/// Journal format version. Bumped if [`JournalEntry`] changes shape; an
/// unrecognised version is treated exactly like a corrupt file.
const JOURNAL_VERSION: u32 = 1;

const JOURNAL_NAME: &str = "duck-journal.json";

// ---------------------------------------------------------------------------
// What a mixer is
// ---------------------------------------------------------------------------

/// Stable identity of one playback stream.
///
/// See the module docs for why this is not an index. Ordering is by serial so a
/// journal round-trips in a stable order and its diffs are readable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StreamKey {
    /// PipeWire's `object.serial`: monotonic for the lifetime of the daemon and
    /// never reused. A backend with nothing better may fall back to the index,
    /// which is why `app` exists.
    pub serial: u64,
    /// `application.name`. Part of the identity, not decoration — it is what
    /// stops a recycled number from being mistaken for the same stream.
    pub app: String,
}

impl StreamKey {
    pub fn new(serial: u64, app: impl Into<String>) -> Self {
        StreamKey { serial, app: app.into() }
    }
}

impl std::fmt::Display for StreamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.app, self.serial)
    }
}

/// One playback stream as the mixer currently sees it.
///
/// `volume` is a single scalar even though the underlying stream may have any
/// number of channels: see [`PactlMixer`] for how a scalar is expanded back out
/// without flattening a stereo balance the operator set on purpose.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub key: StreamKey,
    pub app: String,
    pub volume: f32,
    pub muted: bool,
}

impl StreamInfo {
    pub fn new(key: StreamKey, volume: f32) -> Self {
        let app = key.app.clone();
        StreamInfo { key, app, volume, muted: false }
    }
}

/// Everything the ducker needs from the audio server.
///
/// Two methods, both fallible, neither of which may block for long: this is
/// called on the path that starts a sentence, and a mixer that takes 400 ms to
/// answer is a mixer that makes her late.
pub trait Mixer: Send {
    /// Every stream currently playing to a sink.
    fn playback_streams(&mut self) -> Result<Vec<StreamInfo>>;

    /// Set one stream's volume, `0.0..=1.0` (values above 1.0 are the caller's
    /// problem and backends may clamp).
    fn set_volume(&mut self, key: &StreamKey, vol: f32) -> Result<()>;
}

/// A mixer that does nothing, used only to keep [`Ducker`] non-empty for the
/// instant [`Ducker::recover_owned`] has its real mixer borrowed out.
struct NullMixer;

impl Mixer for NullMixer {
    fn playback_streams(&mut self) -> Result<Vec<StreamInfo>> {
        Err(VoiceError::Mixer("no mixer".into()))
    }
    fn set_volume(&mut self, _key: &StreamKey, _vol: f32) -> Result<()> {
        Err(VoiceError::Mixer("no mixer".into()))
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuckConfig {
    /// How far down, in dB, **relative to each stream's own current volume**.
    /// Negative. A positive value would be a boost and is clamped away: this
    /// module is allowed to make things quieter and nothing else.
    pub attenuation_db: f32,
    /// Ramp length on the way down. Advanced by [`Ducker::tick`]; `0` ducks in
    /// one step. Nothing here fades back *up* — see the module docs.
    pub fade_ms: u32,
    /// Floor. A stream ducked to silence reads as a crash rather than as a
    /// companion talking over it, and it is also the value hardest to notice
    /// has been left behind.
    pub min_volume: f32,
    /// `application.name`s never touched, compared case-insensitively. Ships
    /// with [`OWN_APP`].
    pub exempt: Vec<String>,
}

impl Default for DuckConfig {
    fn default() -> Self {
        DuckConfig {
            // -14 dB is about a fifth of the amplitude: music stays audible and
            // recognisable underneath her, which is what makes it feel like a
            // duck rather than like a pause.
            attenuation_db: -14.0,
            fade_ms: 120,
            min_volume: 0.05,
            exempt: vec![OWN_APP.to_string()],
        }
    }
}

impl DuckConfig {
    /// Linear gain this config applies. Clamped to `..=1.0`.
    pub fn gain(&self) -> f32 {
        if !self.attenuation_db.is_finite() {
            return 1.0;
        }
        from_db(self.attenuation_db).clamp(0.0, 1.0)
    }

    /// What `current` becomes while she is talking.
    ///
    /// The floor never *raises* a stream: something already sitting at 2%
    /// stays at 2% rather than being pushed up to `min_volume`, which would be
    /// this module turning the operator's music up.
    pub fn target_for(&self, current: f32) -> f32 {
        let floor = self.min_volume.clamp(0.0, 1.0).min(current);
        (current * self.gain()).clamp(floor, current)
    }

    fn is_exempt(&self, app: &str) -> bool {
        self.exempt.iter().any(|e| e.eq_ignore_ascii_case(app))
    }
}

// ---------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------

/// One stream's debt, on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub key: StreamKey,
    /// What it was before we touched it. The number that must come back.
    pub original: f32,
    /// The final value the duck is heading for. Recorded for diagnosis; the
    /// restore decision uses `set`.
    pub ducked: f32,
    /// The value we most recently *actually* asked the mixer for. Kept current
    /// through the fade, because a crash halfway down leaves the stream at a
    /// ramp step and the operator-wins comparison has to be against that, not
    /// against the eventual target.
    pub set: f32,
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    /// Who owes the debt.
    pub pid: u32,
    /// `starttime` from `/proc/<pid>/stat`, which disambiguates a recycled pid.
    /// `None` on a platform that has no procfs, which is read as "cannot prove
    /// the owner is alive" and therefore as licence to restore.
    pub pid_start: Option<u64>,
    /// `/proc/sys/kernel/random/boot_id`. A journal from a previous boot names
    /// serials that cannot exist any more, and this is what says so without
    /// having to ask the audio server.
    pub boot_id: Option<String>,
    /// Monotonic ms at the moment of ducking. Never wall clock, and therefore
    /// only ever compared against other values from the same boot.
    pub started_at: Millis,
    pub entries: Vec<JournalEntry>,
}

fn journal_path(dir: &Path) -> PathBuf {
    dir.join(JOURNAL_NAME)
}

fn current_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
}

/// `starttime` (field 22) from `/proc/<pid>/stat`, or `None` if the process is
/// gone or procfs is not there.
///
/// Counted from the last `)` rather than by splitting the whole line: `comm` is
/// the process name unescaped, so a program called `wisp (old)` would otherwise
/// shift every field after it.
fn proc_start_ticks(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &s[s.rfind(')')? + 1..];
    // After the closing paren the next token is field 3 (state), so field 22 is
    // at index 19.
    tail.split_whitespace().nth(19)?.parse().ok()
}

/// Is the process that wrote this journal still running *and not us*?
///
/// The decision this answers: a journal belonging to a **second live instance**
/// must not be replayed. That instance is deliberately holding those streams
/// down right now, and replaying would fight it — she would talk over music
/// that keeps popping back to full. Worse, deleting its journal would destroy
/// the only record that restores those volumes if *it* is the one that gets
/// killed. So we leave the file alone entirely and report it.
///
/// "Not us" matters because a journal written by this very process is an
/// orphan we are entitled — required — to clear. That is exactly the state
/// after a `Ducker` was dropped without releasing, and it is also how the
/// crash test simulates a death without forking.
fn owner_still_running(j: &Journal) -> bool {
    // A journal from another boot cannot have a live owner, whatever the pid
    // table currently says about that number.
    if let (Some(a), Some(b)) = (&j.boot_id, current_boot_id()) {
        if a != &b {
            return false;
        }
    }
    let live_start = proc_start_ticks(j.pid);
    if j.pid == std::process::id() && live_start == j.pid_start {
        return false; // that is this process; the debt is ours to pay
    }
    match live_start {
        // The pid is gone: whoever wrote this is dead and owes us a restore.
        None => false,
        // Same pid, same start time — the original owner is genuinely alive.
        // A different start time means the number was recycled, and the writer
        // is dead after all.
        Some(t) => j.pid_start == Some(t),
    }
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

/// What a restore actually did. Every stream lands in exactly one bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Restored {
    /// Put back to its original volume.
    pub restored: usize,
    /// Left alone because the operator had moved it.
    pub respected: usize,
    /// Not playing any more. Not an error: streams end.
    pub vanished: usize,
    /// The mixer refused. Logged, counted, and not retried.
    pub failed: usize,
}

/// What [`Ducker::recover`] found at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovered {
    /// There was a journal at all.
    pub found: bool,
    /// It was unreadable, and has been deleted. Startup continues.
    pub corrupt: bool,
    /// It belongs to another live instance and was left untouched.
    pub skipped_live_owner: bool,
    pub restore: Restored,
}

impl Recovered {
    /// Nothing to do — the common case, and the one that must be free.
    pub fn clean(&self) -> bool {
        !self.found
    }
}

// ---------------------------------------------------------------------------
// The ducker
// ---------------------------------------------------------------------------

/// Holds other players down for as long as she is talking.
///
/// Owns its [`Mixer`] because [`Drop`] has to be able to restore without being
/// handed one — an early return or a panic-unwind between `duck` and `release`
/// is an ordinary thing for a speech pipeline to do, and it must not cost the
/// operator their volume.
pub struct Ducker {
    mixer: Box<dyn Mixer>,
    cfg: DuckConfig,
    dir: PathBuf,
    /// 0→1 ducks, 1→0 restores. Everything between is free.
    depth: u32,
    entries: Vec<JournalEntry>,
    ramp_start: Millis,
    ramp_done: bool,
    /// Last thing that went wrong, for `wisp doctor`. A mixer that is not there
    /// must never stop her speaking, so failures are recorded, not returned.
    last_error: Option<String>,
}

impl std::fmt::Debug for Ducker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ducker")
            .field("depth", &self.depth)
            .field("ducked", &self.entries.len())
            .field("dir", &self.dir)
            .finish()
    }
}

impl Ducker {
    /// Journals into [`crate::data_dir`], which `NX_WISP_CONFIG_DIR` overrides
    /// so a test can never reach the operator's real state (SPEC §4).
    pub fn new(mixer: Box<dyn Mixer>, cfg: DuckConfig) -> Self {
        let dir = data_dir();
        Ducker::new_in(dir, mixer, cfg)
    }

    /// As [`Ducker::new`], with the journal directory named outright. Two
    /// `Ducker`s over the same directory are two instances sharing one debt,
    /// which is what the crash test uses.
    pub fn new_in(dir: impl Into<PathBuf>, mixer: Box<dyn Mixer>, cfg: DuckConfig) -> Self {
        Ducker {
            mixer,
            cfg,
            dir: dir.into(),
            depth: 0,
            entries: Vec::new(),
            ramp_start: 0,
            ramp_done: true,
            last_error: None,
        }
    }

    pub fn config(&self) -> &DuckConfig {
        &self.cfg
    }

    /// Reconfigure. Takes effect at the next 0→1; changing the attenuation
    /// under a live duck would mean re-deriving targets from volumes that are
    /// already ducked, and compounding attenuation is how a stream ends up at
    /// two percent.
    pub fn set_config(&mut self, cfg: DuckConfig) {
        self.cfg = cfg;
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn is_ducked(&self) -> bool {
        self.depth > 0
    }

    /// The streams this duck is currently holding down.
    pub fn held(&self) -> &[JournalEntry] {
        &self.entries
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn journal_path(&self) -> PathBuf {
        journal_path(&self.dir)
    }

    /// Take one reference on the duck. Only the 0→1 transition touches volumes.
    pub fn duck(&mut self, now: Millis) {
        self.depth = self.depth.saturating_add(1);
        if self.depth != 1 {
            return;
        }
        self.engage(now);
    }

    /// Drop one reference. Only the 1→0 transition restores.
    ///
    /// A release with nothing outstanding is a no-op rather than an underflow:
    /// the ref count is fed by a speech pipeline that can be shed mid-utterance
    /// (SPEC §3.1), so an unpaired release is a thing that happens and must not
    /// wrap the counter to `u32::MAX` and make the duck permanent.
    pub fn release(&mut self, now: Millis) -> Restored {
        if self.depth == 0 {
            return Restored::default();
        }
        self.depth -= 1;
        if self.depth != 0 {
            return Restored::default();
        }
        self.disengage(now)
    }

    /// Drop every reference at once and restore. What a tier downgrade calls:
    /// SPEC §3.1 sheds work, it does not unwind it one clause at a time.
    pub fn release_all(&mut self, now: Millis) -> Restored {
        if self.depth == 0 {
            return Restored::default();
        }
        self.depth = 0;
        self.disengage(now)
    }

    /// Advance the fade. Cheap and idempotent once the ramp has landed, so a
    /// caller may spam it from a frame loop.
    pub fn tick(&mut self, now: Millis) {
        if self.depth == 0 || self.ramp_done {
            return;
        }
        self.apply_ramp(now);
    }

    /// Duck anything that started playing since the duck began.
    ///
    /// Optional on purpose. Nothing calls this automatically, because polling
    /// the mixer on a timer to catch new streams costs a subprocess per tick;
    /// the caller that already watches PipeWire (`wisp_senses::audio`) knows
    /// when a stream appeared and can say so. Streams she never ducked stay
    /// untouched — including on release, since they are not in the journal.
    pub fn refresh(&mut self, now: Millis) -> usize {
        if self.depth == 0 {
            return 0;
        }
        let streams = match self.mixer.playback_streams() {
            Ok(s) => s,
            Err(e) => {
                self.fail("listing streams", e);
                return 0;
            }
        };
        let mut added = 0;
        for s in streams {
            if self.entries.iter().any(|e| e.key == s.key) {
                continue;
            }
            let Some(entry) = self.entry_for(&s) else { continue };
            self.entries.push(entry);
            added += 1;
        }
        if added > 0 {
            self.write_journal(now);
            // A late arrival joins the ramp already in progress rather than
            // getting its own: two overlapping fades on one sink sound like a
            // fault, and she is probably mid-word by now anyway.
            self.ramp_done = false;
            self.apply_ramp(now);
        }
        added
    }

    /// Startup repair. Put back anything a previous life left ducked.
    ///
    /// Takes the mixer explicitly rather than using its own so this can run
    /// before the ducker's real backend is decided — the caller may want to
    /// recover through `pactl` and then speak through PipeWire. Use
    /// [`Ducker::recover_owned`] to run it against the mixer this already has.
    pub fn recover(&mut self, mixer: &mut dyn Mixer) -> Result<Recovered> {
        let path = journal_path(&self.dir);
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Recovered::default()),
            Err(e) => return Err(VoiceError::io(path.display().to_string(), e)),
        };

        let mut out = Recovered { found: true, ..Default::default() };

        // A journal is a repair tool. If the repair tool is broken, the one
        // thing that must not happen is that she refuses to start — the
        // operator would then have neither their volume back nor a companion.
        // Log it, delete it, carry on.
        let journal: Option<Journal> = serde_json::from_slice(&raw).ok();
        let journal = match journal {
            Some(j) if j.version == JOURNAL_VERSION => j,
            other => {
                tracing::warn!(
                    path = %path.display(),
                    bytes = raw.len(),
                    version = other.as_ref().map(|j: &Journal| j.version),
                    "duck journal is unreadable; deleting it and starting clean"
                );
                out.corrupt = true;
                remove_journal(&path);
                return Ok(out);
            }
        };

        if owner_still_running(&journal) {
            tracing::info!(
                pid = journal.pid,
                "duck journal belongs to another running instance; leaving it alone"
            );
            out.skipped_live_owner = true;
            return Ok(out);
        }

        tracing::info!(
            pid = journal.pid,
            streams = journal.entries.len(),
            "recovering volumes from a duck journal that outlived its owner"
        );
        out.restore = restore_entries(mixer, &journal.entries);
        remove_journal(&path);
        Ok(out)
    }

    /// [`Ducker::recover`] against this ducker's own mixer.
    pub fn recover_owned(&mut self) -> Result<Recovered> {
        // The mixer steps out for the length of one call so `recover` can keep
        // the signature that lets a caller supply their own.
        let mut m = std::mem::replace(&mut self.mixer, Box::new(NullMixer));
        let r = self.recover(&mut *m);
        self.mixer = m;
        r
    }

    // -- internals ----------------------------------------------------------

    fn fail(&mut self, what: &str, e: VoiceError) {
        tracing::warn!(error = %e, "duck: {what} failed");
        self.last_error = Some(format!("{what}: {e}"));
    }

    /// Decide what one stream's debt would be, or `None` if it should be left
    /// alone.
    fn entry_for(&self, s: &StreamInfo) -> Option<JournalEntry> {
        if self.cfg.is_exempt(&s.app) {
            return None;
        }
        // A muted stream is already inaudible; ducking it would only give us a
        // volume to restore later and a chance to get that wrong.
        if s.muted {
            return None;
        }
        if !s.volume.is_finite() || s.volume <= 0.0 {
            return None;
        }
        let target = self.cfg.target_for(s.volume);
        if s.volume - target < RAMP_MIN_STEP {
            // Already quiet enough that ducking it is inaudible. Not worth a
            // journal line, and not worth the risk of restoring it wrong.
            return None;
        }
        Some(JournalEntry { key: s.key.clone(), original: s.volume, ducked: target, set: s.volume })
    }

    fn engage(&mut self, now: Millis) {
        self.entries.clear();
        self.ramp_start = now;
        self.ramp_done = false;

        let streams = match self.mixer.playback_streams() {
            Ok(s) => s,
            Err(e) => {
                // No audio server, or `pactl` is not installed. She still gets
                // to talk; the machine is simply louder than ideal.
                self.fail("listing streams", e);
                self.ramp_done = true;
                return;
            }
        };
        self.entries = streams.iter().filter_map(|s| self.entry_for(s)).collect();
        if self.entries.is_empty() {
            self.ramp_done = true;
            return;
        }

        // **Before** a single volume moves. If the process dies between this
        // line and the first `set_volume`, recovery restores values that were
        // never changed, which is a no-op. If it dies after, the journal is
        // already there. There is no ordering of these two that loses.
        self.write_journal(now);
        self.apply_ramp(now);
    }

    fn apply_ramp(&mut self, now: Millis) {
        let p = if self.cfg.fade_ms == 0 {
            1.0
        } else {
            let elapsed = now.saturating_sub(self.ramp_start) as f32;
            (elapsed / self.cfg.fade_ms as f32).clamp(0.0, 1.0)
        };
        let landed = p >= 1.0;

        let mut moved = false;
        for i in 0..self.entries.len() {
            let (key, want, last) = {
                let e = &self.entries[i];
                let want = e.original + (e.ducked - e.original) * p;
                (e.key.clone(), want, e.set)
            };
            let delta = (want - last).abs();
            if delta < RAMP_MIN_STEP && !(landed && delta > f32::EPSILON) {
                continue;
            }
            match self.mixer.set_volume(&key, want) {
                Ok(()) => {
                    self.entries[i].set = want;
                    moved = true;
                }
                Err(e) => {
                    // We never managed to move it, so as far as the restore is
                    // concerned it is still at its original value. Recording
                    // that keeps the operator-wins comparison honest instead of
                    // relying on it to reach the right answer by accident.
                    self.entries[i].set = self.entries[i].original;
                    self.fail(&format!("setting volume of {key}"), e);
                    moved = true;
                }
            }
        }

        if moved {
            // The journal has to track the ramp, not the destination: a crash
            // at 40% of the way down must restore from where the stream
            // actually is.
            self.write_journal(now);
        }
        self.ramp_done = landed;
    }

    fn disengage(&mut self, _now: Millis) -> Restored {
        if self.entries.is_empty() {
            remove_journal(&journal_path(&self.dir));
            return Restored::default();
        }
        let entries = std::mem::take(&mut self.entries);
        let out = restore_entries(&mut *self.mixer, &entries);
        remove_journal(&journal_path(&self.dir));
        self.ramp_done = true;
        if out.failed > 0 {
            self.last_error = Some(format!("{} stream(s) could not be restored", out.failed));
        }
        out
    }

    fn write_journal(&mut self, now: Millis) {
        let j = Journal {
            version: JOURNAL_VERSION,
            pid: std::process::id(),
            pid_start: proc_start_ticks(std::process::id()),
            boot_id: current_boot_id(),
            started_at: now,
            entries: self.entries.clone(),
        };
        if let Err(e) = write_journal_to(&journal_path(&self.dir), &j) {
            // Worth shouting about: from here on a crash costs the operator
            // their volume, which is the one thing this module promises not to
            // do. It is still not worth refusing to speak over.
            tracing::error!(error = %e, "could not write the duck journal; a crash now would leave volumes down");
            self.last_error = Some(format!("journal: {e}"));
        }
    }
}

impl Drop for Ducker {
    /// The safety net for the ordinary paths — an early `?`, a panic unwind, a
    /// task cancelled mid-sentence. A `SIGKILL` skips this, which is what the
    /// journal is for.
    fn drop(&mut self) {
        if self.depth == 0 && self.entries.is_empty() {
            return;
        }
        self.depth = 0;
        let entries = std::mem::take(&mut self.entries);
        if !entries.is_empty() {
            let out = restore_entries(&mut *self.mixer, &entries);
            tracing::debug!(?out, "ducker dropped while ducked; volumes restored");
        }
        remove_journal(&journal_path(&self.dir));
    }
}

/// The one restore implementation, shared by `release`, `Drop` and `recover`.
///
/// Lists first, always. That list is what makes "the operator moved it" and
/// "the stream is gone" distinguishable, and it is also what keeps a backend's
/// serial→index cache fresh enough that a set cannot land on a recycled index.
fn restore_entries(mixer: &mut dyn Mixer, entries: &[JournalEntry]) -> Restored {
    let mut out = Restored::default();

    let live: Option<HashMap<StreamKey, StreamInfo>> = match mixer.playback_streams() {
        Ok(v) => Some(v.into_iter().map(|s| (s.key.clone(), s)).collect()),
        Err(e) => {
            // Restore blind. Yes, this can overwrite a volume the operator
            // changed — but the alternative is leaving music turned down with
            // the journal deleted, and between "possibly one stream back where
            // she found it" and "permanently quiet music" the module thesis
            // picks the first without hesitating.
            tracing::warn!(error = %e, "cannot list streams; restoring blind");
            None
        }
    };

    for e in entries {
        match &live {
            Some(map) => match map.get(&e.key) {
                None => {
                    // It ended while she was talking. Nothing to put back.
                    out.vanished += 1;
                }
                Some(now) => {
                    if (now.volume - e.set).abs() > OPERATOR_EPSILON {
                        tracing::debug!(
                            key = %e.key,
                            ours = e.set,
                            theirs = now.volume,
                            "volume moved while ducked; the operator wins"
                        );
                        out.respected += 1;
                    } else if let Err(err) = mixer.set_volume(&e.key, e.original) {
                        tracing::warn!(key = %e.key, error = %err, "could not restore volume");
                        out.failed += 1;
                    } else {
                        out.restored += 1;
                    }
                }
            },
            None => {
                if mixer.set_volume(&e.key, e.original).is_ok() {
                    out.restored += 1;
                } else {
                    out.failed += 1;
                }
            }
        }
    }
    out
}

fn write_journal_to(path: &Path, j: &Journal) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let bytes = serde_json::to_vec_pretty(j)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Write-then-rename, as `wisp_senses::consent` does: a journal truncated by
    // the very crash it exists to survive would be worse than no journal, since
    // it would also be *believed* until it failed to parse.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)
}

fn remove_journal(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "could not delete the duck journal"),
    }
}

// ---------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------

/// An in-memory audio server. Always compiled and `pub`, so the binary can run
/// her ducking logic on a machine with no PipeWire at all and so every test in
/// this module runs without one.
///
/// It does the four things that actually go wrong in the field: a stream
/// disappears, a stream appears, a set is refused, and the operator moves a
/// slider.
///
/// **`Clone` is a second handle on the same server, not a copy of it.** That is
/// what makes the crash test honest: PipeWire does not die when she does, so
/// the volumes have to outlive the [`Ducker`] that changed them.
#[derive(Debug, Clone, Default)]
pub struct FakeMixer {
    inner: std::sync::Arc<std::sync::Mutex<FakeState>>,
}

#[derive(Debug, Default)]
struct FakeState {
    streams: Vec<StreamInfo>,
    lists: usize,
    sets: usize,
    set_log: Vec<(StreamKey, f32)>,
    fail_list: bool,
    fail_sets: Vec<StreamKey>,
}

impl FakeMixer {
    pub fn new() -> Self {
        FakeMixer::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
        // Poisoning is ignored: a test that panicked mid-assertion should fail
        // on its own message, not turn every later lock into a second failure.
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }

    /// Builder form: `FakeMixer::new().playing("Firefox", 3148, 0.75)`.
    pub fn playing(self, app: &str, serial: u64, vol: f32) -> Self {
        self.add(app, serial, vol);
        self
    }

    /// A stream starts.
    pub fn add(&self, app: &str, serial: u64, vol: f32) -> StreamKey {
        let key = StreamKey::new(serial, app);
        self.with(|s| s.streams.push(StreamInfo::new(key.clone(), vol)));
        key
    }

    /// A stream ends.
    pub fn remove(&self, key: &StreamKey) {
        self.with(|s| s.streams.retain(|x| &x.key != key));
    }

    /// The operator reaches for the knob. Deliberately not routed through
    /// [`Mixer::set_volume`]: it leaves the counters alone, so a test can tell
    /// her writes from theirs.
    pub fn operator_sets(&self, key: &StreamKey, vol: f32) {
        self.with(|s| {
            if let Some(x) = s.streams.iter_mut().find(|x| &x.key == key) {
                x.volume = vol;
            }
        });
    }

    pub fn set_muted(&self, key: &StreamKey, muted: bool) {
        self.with(|s| {
            if let Some(x) = s.streams.iter_mut().find(|x| &x.key == key) {
                x.muted = muted;
            }
        });
    }

    pub fn volume_of(&self, key: &StreamKey) -> Option<f32> {
        self.with(|s| s.streams.iter().find(|x| &x.key == key).map(|x| x.volume))
    }

    /// Snapshot, sorted, for an exact before/after comparison.
    pub fn volumes(&self) -> Vec<(StreamKey, f32)> {
        self.with(|s| {
            let mut v: Vec<(StreamKey, f32)> =
                s.streams.iter().map(|x| (x.key.clone(), x.volume)).collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        })
    }

    /// Calls to [`Mixer::playback_streams`].
    pub fn lists(&self) -> usize {
        self.with(|s| s.lists)
    }

    /// Sets that were accepted.
    pub fn sets(&self) -> usize {
        self.with(|s| s.sets)
    }

    /// Every set that was asked for, in order, accepted or not.
    pub fn set_log(&self) -> Vec<(StreamKey, f32)> {
        self.with(|s| s.set_log.clone())
    }

    /// There is no audio server.
    pub fn fail_lists(&self, yes: bool) {
        self.with(|s| s.fail_list = yes);
    }

    /// This one stream refuses to be set.
    pub fn fail_sets_for(&self, key: &StreamKey) {
        self.with(|s| s.fail_sets.push(key.clone()));
    }
}

impl Mixer for FakeMixer {
    fn playback_streams(&mut self) -> Result<Vec<StreamInfo>> {
        self.with(|s| {
            s.lists += 1;
            if s.fail_list {
                return Err(VoiceError::Mixer("FakeMixer: there is no audio server".into()));
            }
            Ok(s.streams.clone())
        })
    }

    fn set_volume(&mut self, key: &StreamKey, vol: f32) -> Result<()> {
        self.with(|s| {
            s.set_log.push((key.clone(), vol));
            if s.fail_sets.contains(key) {
                return Err(VoiceError::Mixer(format!("FakeMixer refuses to set {key}")));
            }
            match s.streams.iter_mut().find(|x| &x.key == key) {
                Some(x) => {
                    x.volume = vol.clamp(0.0, 1.0);
                    s.sets += 1;
                    Ok(())
                }
                None => Err(VoiceError::Mixer(format!("{key} is gone"))),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// The real one: pactl
// ---------------------------------------------------------------------------

/// One sink-input, exactly as `pactl -f json list sink-inputs` describes it.
///
/// Richer than [`StreamInfo`] because the *write* path needs two things the
/// trait's scalar cannot carry: the pulse `index` (which is what
/// `set-sink-input-volume` addresses) and the per-channel volumes (which is how
/// a balance survives a duck).
#[derive(Debug, Clone, PartialEq)]
pub struct SinkInput {
    /// The pulse index. Addressable, **and recycled** — never an identity.
    pub index: u32,
    /// `properties["object.serial"]`, parsed. Falls back to `index` when the
    /// server does not supply one, which is the only case where the app-name
    /// half of [`StreamKey`] is doing real work.
    pub serial: u64,
    pub app: String,
    /// `properties["media.name"]` — "Tracks - TIDAL", "audio stream #1". Not
    /// part of the identity: it changes when the track changes.
    pub media_name: String,
    pub muted: bool,
    pub corked: bool,
    /// One entry per channel, **in `channel_map` order**, `1.0` = 100%.
    pub channels: Vec<f32>,
}

impl SinkInput {
    pub fn key(&self) -> StreamKey {
        StreamKey::new(self.serial, self.app.clone())
    }

    /// The scalar the [`Mixer`] trait deals in.
    ///
    /// The **max** across channels, not the mean. A stream panned hard left is
    /// as loud as its loudest channel, and taking the mean would report a
    /// stereo stream with one silent side as half volume and then duck it from
    /// there — quieter than asked for, and restored to a different balance.
    pub fn volume(&self) -> f32 {
        self.channels.iter().copied().fold(0.0f32, |m, v| if v.is_finite() && v > m { v } else { m })
    }

    pub fn info(&self) -> StreamInfo {
        StreamInfo {
            key: self.key(),
            app: self.app.clone(),
            volume: self.volume(),
            muted: self.muted,
        }
    }
}

/// Parse `pactl -f json list sink-inputs`.
///
/// A pure function over a string so the shape can be tested against captured
/// output from a real machine rather than against a guess — see the fixture in
/// this module's tests, which is verbatim `pactl 17.0` under `pipewire-pulse`.
///
/// **Channel order comes from `channel_map`, never from the JSON object.**
/// `serde_json` without `preserve_order` stores an object in a `BTreeMap`, so
/// `volume` arrives alphabetically: for 5.1 that is `front-center, front-left,
/// front-right, lfe, …` while the real channel order is `front-left,
/// front-right, front-center, lfe, …`. Writing those back positionally would
/// silently swap the centre and left channels of anything above stereo.
///
/// One unparseable entry is skipped rather than failing the whole list: a
/// stream she cannot understand should cost her that stream's duck, not all of
/// them.
pub fn parse_sink_inputs(json: &str) -> Result<Vec<SinkInput>> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| VoiceError::Mixer(format!("pactl did not emit JSON: {e}")))?;
    let arr = v
        .as_array()
        .ok_or_else(|| VoiceError::Mixer("pactl sink-input list was not an array".into()))?;
    Ok(arr.iter().filter_map(parse_sink_input).collect())
}

fn parse_sink_input(v: &serde_json::Value) -> Option<SinkInput> {
    let index = v.get("index")?.as_u64()? as u32;
    let props = v.get("properties").and_then(|p| p.as_object());
    let prop = |k: &str| props.and_then(|p| p.get(k)).and_then(|x| x.as_str());

    let serial = prop("object.serial").and_then(|s| s.parse::<u64>().ok()).unwrap_or(index as u64);

    // `application.name` is what a person would recognise. The fallbacks are
    // ordered by how close each is to that: the node name is usually a copy of
    // it, and the binary name is at least stable.
    let app = prop("application.name")
        .or_else(|| prop("node.name"))
        .or_else(|| prop("application.process.binary"))
        .unwrap_or("unknown")
        .to_string();

    let media_name = prop("media.name").unwrap_or_default().to_string();
    let muted = v.get("mute").and_then(|m| m.as_bool()).unwrap_or(false);
    let corked = v.get("corked").and_then(|m| m.as_bool()).unwrap_or(false);

    let vol = v.get("volume")?.as_object()?;
    let order: Vec<&str> = v
        .get("channel_map")
        .and_then(|c| c.as_str())
        .map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();

    let channels: Vec<f32> = if order.len() == vol.len() && order.iter().all(|n| vol.contains_key(*n))
    {
        order.iter().map(|n| channel_volume(&vol[*n])).collect()
    } else {
        // No usable channel map. Alphabetical order is a guess, so flatten to
        // one value instead of writing a guessed permutation back to the
        // server — a mono-ised duck is recoverable, a swapped centre channel is
        // not obviously anybody's fault.
        let max = vol.values().map(channel_volume).fold(0.0f32, f32::max);
        vec![max; vol.len().max(1)]
    };

    Some(SinkInput { index, serial, app, media_name, muted, corked, channels })
}

/// One channel of a `volume` map. Prefers the raw `value` over `value_percent`,
/// which is already rounded to whole percent by the time it is printed.
fn channel_volume(v: &serde_json::Value) -> f32 {
    if let Some(raw) = v.get("value").and_then(|x| x.as_f64()) {
        return (raw / VOLUME_NORM) as f32;
    }
    v.get("value_percent")
        .and_then(|x| x.as_str())
        .and_then(|s| s.trim_end_matches('%').trim().parse::<f32>().ok())
        .map(|p| p / 100.0)
        .unwrap_or(0.0)
}

/// What one stream looked like at the last listing, kept so a scalar
/// `set_volume` can be expanded back to per-channel values.
#[derive(Debug, Clone)]
struct Cached {
    index: u32,
    channels: Vec<f32>,
    /// The scalar those channels reduce to. The ratio target/max is what gets
    /// applied, which is what preserves balance.
    max: f32,
}

/// The shipping backend: `pactl`, over the PulseAudio compatibility layer that
/// `pipewire-pulse` provides.
///
/// **Why `pactl` and not the `pipewire` crate**, given `pipewire-backend` is
/// right there in `Cargo.toml`: the native API is an event loop you must own.
/// `pipewire::MainLoop` is not `Send`, it wants to run on its own thread for
/// the lifetime of the connection, and reading a node's volume means round-
/// tripping a `Spa` pod through a registry listener. All of that is the right
/// shape for the *sense* that watches levels continuously
/// (`wisp_senses::audio` already pays for it) and the wrong shape for a
/// module whose entire interaction with the server is "twice per sentence, tell
/// me the volumes and set a few". Shelling out costs about 8 ms and buys a
/// backend with no threads, no lifetime, and no way to wedge her speech path.
/// If the duck ever needs to follow volume *changes* live, that is the point at
/// which the native backend earns its complexity — not before.
pub struct PactlMixer {
    bin: String,
    cache: HashMap<u64, Cached>,
    /// Log what would be set and change nothing. The safe way to point this at
    /// a machine somebody is using.
    pub dry_run: bool,
}

impl Default for PactlMixer {
    fn default() -> Self {
        PactlMixer::new()
    }
}

impl PactlMixer {
    pub fn new() -> Self {
        PactlMixer { bin: "pactl".to_string(), cache: HashMap::new(), dry_run: false }
    }

    /// Reads volumes, never writes them.
    pub fn probe() -> Self {
        PactlMixer { dry_run: true, ..PactlMixer::new() }
    }

    /// For a machine where `pactl` is not on `PATH` under that name.
    pub fn with_binary(bin: impl Into<String>) -> Self {
        PactlMixer { bin: bin.into(), ..PactlMixer::new() }
    }

    /// Is there anything to talk to at all? Used by `wisp doctor`.
    pub fn available(&mut self) -> bool {
        self.list().is_ok()
    }

    fn list(&mut self) -> Result<Vec<SinkInput>> {
        let out = std::process::Command::new(&self.bin)
            .args(["-f", "json", "list", "sink-inputs"])
            .output()
            .map_err(|e| VoiceError::Mixer(format!("running {}: {e}", self.bin)))?;
        if !out.status.success() {
            // `pactl` puts "Connection refused" on stderr and exits 1 when
            // there is no server. That is a normal state on a headless box, not
            // a bug, so the message is carried rather than dressed up.
            return Err(VoiceError::Mixer(format!(
                "{} exited {}: {}",
                self.bin,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let json = String::from_utf8_lossy(&out.stdout);
        let inputs = parse_sink_inputs(&json)?;

        self.cache.clear();
        for i in &inputs {
            self.cache
                .insert(i.serial, Cached { index: i.index, channels: i.channels.clone(), max: i.volume() });
        }
        Ok(inputs)
    }
}

impl Mixer for PactlMixer {
    fn playback_streams(&mut self) -> Result<Vec<StreamInfo>> {
        Ok(self.list()?.iter().map(SinkInput::info).collect())
    }

    /// Writes every channel, scaled by `vol / max`, so a stream the operator
    /// panned stays panned. A restore therefore reproduces the original
    /// per-channel values exactly, because the ratio comes back to 1.0.
    fn set_volume(&mut self, key: &StreamKey, vol: f32) -> Result<()> {
        if !self.cache.contains_key(&key.serial) {
            // Unknown serial: re-list rather than guess. This is also the only
            // thing standing between us and a recycled index, so it is not an
            // optimisation to skip.
            self.list()?;
        }
        let cached = self
            .cache
            .get(&key.serial)
            .ok_or_else(|| VoiceError::Mixer(format!("{key} is no longer playing")))?
            .clone();

        let vol = vol.clamp(0.0, 1.0);
        let ratio = if cached.max > 0.0 { vol / cached.max } else { 1.0 };
        let mut args: Vec<String> = vec!["set-sink-input-volume".into(), cached.index.to_string()];
        for c in &cached.channels {
            let v = if cached.max > 0.0 { c * ratio } else { vol };
            // Whole percent: it is what `pactl` parses most reliably across
            // versions and it is what the operator would have typed. The
            // rounding is under `OPERATOR_EPSILON` by an order of magnitude.
            args.push(format!("{}%", (v.clamp(0.0, 1.0) * 100.0).round() as u32));
        }

        if self.dry_run {
            tracing::info!(key = %key, args = ?args, "dry run: not setting a real volume");
            return Ok(());
        }

        let out = std::process::Command::new(&self.bin)
            .args(&args)
            .output()
            .map_err(|e| VoiceError::Mixer(format!("running {}: {e}", self.bin)))?;
        if !out.status.success() {
            return Err(VoiceError::Mixer(format!(
                "{} {}: {}",
                self.bin,
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// SPEC §4's rule — `NX_WISP_CONFIG_DIR` points at a temp directory before
    /// any test can reach real state — applied **once for the whole test
    /// binary** rather than once per test.
    ///
    /// The obvious shape (set it in `new`, restore it in `Drop`) is not
    /// thread-safe, and the suite is threaded. `set_var` mutates process-global
    /// state, and this crate has another test that reads it —
    /// `lib.rs`'s `data_dir_honours_the_test_override` sets the variable,
    /// asserts on [`data_dir`] and restores it. Any test flipping the same
    /// variable in that window makes it fail; forty tests doing it twice each
    /// made the suite flake roughly one run in a hundred. Setting it exactly
    /// once, at the first use, and never moving it again is the version of the
    /// rule that survives `cargo test`'s thread pool.
    ///
    /// Every `Ducker` below is additionally built with [`Ducker::new_in`] and
    /// its own [`tempfile::TempDir`], so the operator's real store is
    /// unreachable twice over rather than once — and so the directory the
    /// variable names is never written to, which is why it is not created. A
    /// static holding a real `TempDir` would never be dropped, and would leave
    /// one empty directory in `/tmp` for every `cargo test` the operator ever
    /// runs.
    fn point_the_store_at_a_temp_dir() {
        static ONCE: OnceLock<PathBuf> = OnceLock::new();
        ONCE.get_or_init(|| {
            let p = std::env::temp_dir().join(format!("nx-wisp-voice-tests-{}", std::process::id()));
            std::env::set_var("NX_WISP_CONFIG_DIR", &p);
            p
        });
    }

    /// One test's journal directory, deleted when the test ends.
    struct TempStore {
        dir: tempfile::TempDir,
    }

    impl TempStore {
        fn new() -> Self {
            point_the_store_at_a_temp_dir();
            TempStore { dir: tempfile::tempdir().unwrap() }
        }
        fn path(&self) -> &Path {
            self.dir.path()
        }
        fn journal(&self) -> PathBuf {
            journal_path(self.dir.path())
        }
    }

    /// No fade, so a test about ref counting is not also a test about ramps.
    fn instant() -> DuckConfig {
        DuckConfig { fade_ms: 0, ..Default::default() }
    }

    /// The ducker owns its mixer (so `Drop` can restore); the test keeps a
    /// second handle on the same fake server, exactly as the real audio server
    /// is a thing outside the process.
    fn ducker(store: &TempStore, audio: &FakeMixer, cfg: DuckConfig) -> Ducker {
        Ducker::new_in(store.path(), Box::new(audio.clone()), cfg)
    }

    // -- the ref count ------------------------------------------------------

    #[test]
    fn two_clauses_back_to_back_are_one_duck_and_only_the_last_release_restores() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 3148, 0.8);
        let key = StreamKey::new(3148, "Firefox");
        let mut d = ducker(&store, &audio, instant());

        d.duck(0);
        let ducked = audio.volume_of(&key).unwrap();
        assert!(ducked < 0.8, "the first duck must actually duck");

        d.duck(10);
        assert_eq!(d.depth(), 2);
        assert_eq!(
            audio.volume_of(&key).unwrap(),
            ducked,
            "the second duck must not attenuate a second time"
        );

        d.release(20);
        assert_eq!(d.depth(), 1);
        assert_eq!(
            audio.volume_of(&key).unwrap(),
            ducked,
            "she is still talking; the music stays down"
        );

        let out = d.release(30);
        assert_eq!(d.depth(), 0);
        assert_eq!(out.restored, 1);
        assert_eq!(audio.volume_of(&key).unwrap(), 0.8);
    }

    #[test]
    fn a_release_with_nothing_outstanding_is_a_no_op_rather_than_an_underflow() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let mut d = ducker(&store, &audio, instant());
        d.release(0);
        d.release(1);
        assert_eq!(d.depth(), 0, "the count must not wrap and make the duck permanent");
        assert_eq!(audio.volume_of(&StreamKey::new(1, "Firefox")).unwrap(), 0.8);
    }

    #[test]
    fn release_all_drops_every_reference_at_once() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let key = StreamKey::new(1, "Firefox");
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        d.duck(1);
        d.duck(2);
        let out = d.release_all(3);
        assert_eq!(d.depth(), 0);
        assert_eq!(out.restored, 1);
        assert_eq!(audio.volume_of(&key).unwrap(), 0.8);
    }

    // -- what gets ducked, and by how much ----------------------------------

    #[test]
    fn her_own_output_stream_is_never_ducked() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing(OWN_APP, 10, 0.9).playing("Firefox", 11, 0.9);
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        assert_eq!(
            audio.volume_of(&StreamKey::new(10, OWN_APP)).unwrap(),
            0.9,
            "ducking herself would make her quieter every time she spoke"
        );
        assert!(audio.volume_of(&StreamKey::new(11, "Firefox")).unwrap() < 0.9);
    }

    #[test]
    fn the_exempt_list_is_case_insensitive() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.9);
        let cfg = DuckConfig { exempt: vec!["firefox".into()], ..instant() };
        let mut d = ducker(&store, &audio, cfg);
        d.duck(0);
        assert_eq!(audio.volume_of(&StreamKey::new(1, "Firefox")).unwrap(), 0.9);
    }

    #[test]
    fn a_muted_stream_is_left_alone_rather_than_journalled() {
        let store = TempStore::new();
        let audio = FakeMixer::new();
        let key = audio.add("Discord", 354, 1.0);
        audio.set_muted(&key, true);
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        assert!(d.held().is_empty(), "a silent stream is not a debt worth taking on");
        assert_eq!(audio.volume_of(&key).unwrap(), 1.0);
    }

    #[test]
    fn attenuation_is_relative_to_each_streams_own_volume() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Loud", 1, 1.0).playing("Quiet", 2, 0.4);
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        let loud = audio.volume_of(&StreamKey::new(1, "Loud")).unwrap();
        let quiet = audio.volume_of(&StreamKey::new(2, "Quiet")).unwrap();
        let g = DuckConfig::default().gain();
        assert!((loud - g).abs() < 1e-4, "{loud} vs {g}");
        assert!((quiet - 0.4 * g).abs() < 1e-4, "{quiet}");
        assert!(quiet < loud, "the quiet stream must not end up louder than the loud one");
    }

    #[test]
    fn ducking_never_goes_below_the_floor_and_never_raises_anything() {
        let cfg = DuckConfig { min_volume: 0.1, ..Default::default() };
        assert!((cfg.target_for(1.0) - cfg.gain()).abs() < 1e-6);
        assert_eq!(cfg.target_for(0.12), 0.1, "the floor holds");
        assert_eq!(cfg.target_for(0.02), 0.02, "the floor must never turn the music up");
        assert_eq!(cfg.target_for(0.0), 0.0);
    }

    #[test]
    fn a_positive_attenuation_is_refused_rather_than_boosting_the_operators_music() {
        let cfg = DuckConfig { attenuation_db: 6.0, ..Default::default() };
        assert_eq!(cfg.gain(), 1.0);
        assert_eq!(cfg.target_for(0.5), 0.5);
    }

    #[test]
    fn a_stream_already_quiet_enough_is_not_touched_at_all() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.51);
        let cfg = DuckConfig { min_volume: 0.5, ..instant() };
        let mut d = ducker(&store, &audio, cfg);
        d.duck(0);
        assert!(d.held().is_empty());
        assert!(!store.journal().exists(), "nothing was touched, so there is no debt to record");
    }

    // -- the operator wins --------------------------------------------------

    #[test]
    fn a_volume_the_operator_moved_while_ducked_is_not_stomped_on_restore() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 3148, 0.8);
        let key = StreamKey::new(3148, "Firefox");
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);

        // They reached for the knob while she was mid-sentence.
        audio.operator_sets(&key, 0.35);

        let out = d.release(100);
        assert_eq!(out.respected, 1);
        assert_eq!(out.restored, 0);
        assert_eq!(audio.volume_of(&key).unwrap(), 0.35, "their number is the truth now");
    }

    #[test]
    fn a_percent_of_rounding_is_still_us_and_gets_restored() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let key = StreamKey::new(1, "Firefox");
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        let ducked = audio.volume_of(&key).unwrap();
        // What a round trip through `pactl`'s whole percents looks like.
        audio.operator_sets(&key, (ducked * 100.0).round() / 100.0);
        let out = d.release(50);
        assert_eq!(out.restored, 1, "quantisation must not read as a human");
        assert_eq!(audio.volume_of(&key).unwrap(), 0.8);
    }

    // -- streams coming and going -------------------------------------------

    #[test]
    fn a_stream_that_ended_mid_sentence_is_skipped_without_an_error() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8).playing("Discord", 2, 0.6);
        let gone = StreamKey::new(1, "Firefox");
        let stays = StreamKey::new(2, "Discord");
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        audio.remove(&gone);

        let out = d.release(100);
        assert_eq!(out.vanished, 1);
        assert_eq!(out.restored, 1);
        assert_eq!(out.failed, 0, "a stream that ended is not a failure");
        assert_eq!(audio.volume_of(&stays).unwrap(), 0.6);
    }

    #[test]
    fn a_stream_that_appeared_mid_duck_is_left_exactly_where_it_started() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        let late = audio.add("Spotify", 2, 0.55);
        d.release(100);
        assert_eq!(
            audio.volume_of(&late).unwrap(),
            0.55,
            "she never ducked it, so restore must not invent a value for it"
        );
    }

    #[test]
    fn refresh_ducks_a_stream_that_started_playing_after_she_did() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        let late = audio.add("Spotify", 2, 0.6);
        assert_eq!(d.refresh(10), 1);
        assert!(audio.volume_of(&late).unwrap() < 0.6);

        d.release(100);
        assert_eq!(audio.volume_of(&late).unwrap(), 0.6, "and it comes back with everything else");
    }

    #[test]
    fn refresh_outside_a_duck_does_nothing() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let mut d = ducker(&store, &audio, instant());
        assert_eq!(d.refresh(0), 0);
        assert_eq!(audio.volume_of(&StreamKey::new(1, "Firefox")).unwrap(), 0.8);
    }

    #[test]
    fn a_recycled_serial_belonging_to_a_different_app_is_treated_as_a_new_stream() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let firefox = StreamKey::new(1, "Firefox");
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        assert!(audio.volume_of(&firefox).unwrap() < 0.8);

        // Firefox closes and something else inherits the number.
        audio.remove(&firefox);
        let impostor = audio.add("Spotify", 1, 0.45);

        let out = d.release(100);
        assert_eq!(out.vanished, 1, "the app name is part of the identity for exactly this");
        assert_eq!(
            audio.volume_of(&impostor).unwrap(),
            0.45,
            "restoring onto a recycled number is the bug this module exists to prevent"
        );
    }

    // -- the mixer failing --------------------------------------------------

    #[test]
    fn a_set_that_fails_still_leaves_the_other_streams_ducked_and_restorable() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8).playing("Discord", 2, 0.6);
        let bad = StreamKey::new(1, "Firefox");
        let good = StreamKey::new(2, "Discord");
        audio.fail_sets_for(&bad);
        let mut d = ducker(&store, &audio, instant());

        d.duck(0);
        assert_eq!(audio.volume_of(&bad).unwrap(), 0.8, "the refused one never moved");
        assert!(audio.volume_of(&good).unwrap() < 0.6);
        assert!(d.last_error().is_some());

        let out = d.release(100);
        assert_eq!(audio.volume_of(&bad).unwrap(), 0.8, "and it is still where it started");
        assert_eq!(audio.volume_of(&good).unwrap(), 0.6);
        assert_eq!(out.restored, 1);
        assert_eq!(out.failed, 1, "the stream that refuses her writes refuses this one too");
    }

    #[test]
    fn no_audio_server_at_all_does_not_stop_her_talking() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        audio.fail_lists(true);
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        assert!(d.held().is_empty());
        assert!(d.last_error().unwrap().contains("listing streams"));
        d.release(10);
        assert!(!store.journal().exists());
    }

    #[test]
    fn a_restore_that_cannot_list_puts_the_volumes_back_blind() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let key = StreamKey::new(1, "Firefox");
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        assert!(audio.volume_of(&key).unwrap() < 0.8);
        audio.fail_lists(true);

        let out = d.release(100);
        assert_eq!(out.restored, 1);
        assert_eq!(
            audio.volume_of(&key).unwrap(),
            0.8,
            "quiet music forever is worse than one possibly-stomped slider"
        );
    }

    // -- the fade -----------------------------------------------------------

    #[test]
    fn the_fade_walks_down_and_lands_exactly_on_the_target() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 1.0);
        let key = StreamKey::new(1, "Firefox");
        let mut d = ducker(&store, &audio, DuckConfig { fade_ms: 100, ..Default::default() });

        d.duck(1000);
        assert_eq!(audio.volume_of(&key).unwrap(), 1.0, "at t=0 the ramp has not moved yet");
        d.tick(1050);
        let mid = audio.volume_of(&key).unwrap();
        assert!(mid < 1.0 && mid > DuckConfig::default().gain(), "halfway: {mid}");
        d.tick(1100);
        let end = audio.volume_of(&key).unwrap();
        assert!((end - DuckConfig::default().gain()).abs() < 1e-4, "{end}");

        let before = audio.sets();
        d.tick(1200);
        d.tick(5000);
        assert_eq!(audio.sets(), before, "a landed ramp must stop spending subprocesses");
    }

    #[test]
    fn a_release_during_the_fade_still_restores_the_original() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.9);
        let key = StreamKey::new(1, "Firefox");
        let mut d = ducker(&store, &audio, DuckConfig { fade_ms: 200, ..Default::default() });
        d.duck(0);
        d.tick(60);
        assert!(audio.volume_of(&key).unwrap() < 0.9);
        d.release(61);
        assert_eq!(audio.volume_of(&key).unwrap(), 0.9);
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_invert_the_fade() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 1.0);
        let key = StreamKey::new(1, "Firefox");
        let mut d = ducker(&store, &audio, DuckConfig { fade_ms: 100, ..Default::default() });
        d.duck(5000);
        d.tick(10); // impossible, but a saturating_sub has to make it harmless
        assert_eq!(audio.volume_of(&key).unwrap(), 1.0);
    }

    // -- the journal, and the headline test ---------------------------------

    #[test]
    fn the_journal_is_written_before_a_volume_moves_and_deleted_on_a_clean_release() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 3148, 0.8);
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        assert!(store.journal().exists(), "the debt has to be on disk before it is incurred");

        let j: Journal = serde_json::from_slice(&std::fs::read(store.journal()).unwrap()).unwrap();
        assert_eq!(j.version, JOURNAL_VERSION);
        assert_eq!(j.pid, std::process::id());
        assert_eq!(j.entries.len(), 1);
        assert_eq!(j.entries[0].key, StreamKey::new(3148, "Firefox"));
        assert_eq!(j.entries[0].original, 0.8);
        assert!(j.entries[0].ducked < 0.8);

        d.release(50);
        assert!(!store.journal().exists());
    }

    /// The headline. She is `SIGKILL`ed mid-sentence with the music turned
    /// down; the next start must put it back exactly.
    ///
    /// The death is `mem::forget`: it skips the `Drop` that would otherwise
    /// restore, which is exactly what a signal does. The `FakeMixer` handle the
    /// test holds is the *same* audio server, so — as in life — the volumes
    /// stay down after she is gone. Then a fresh `Ducker` over the same journal
    /// directory stands in for the relaunch.
    #[test]
    fn being_killed_mid_sentence_leaves_a_journal_that_the_next_start_pays_off() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 3148, 0.8).playing("Discord", 354, 0.55);
        let firefox = StreamKey::new(3148, "Firefox");
        let discord = StreamKey::new(354, "Discord");
        let before = audio.volumes();

        // --- her first life ---
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        assert!(audio.volume_of(&firefox).unwrap() < 0.8);
        assert!(audio.volume_of(&discord).unwrap() < 0.55);
        assert!(store.journal().exists());

        std::mem::forget(d); // SIGKILL: no Drop, no release, no cleanup.
        assert!(store.journal().exists(), "the journal must survive her");
        assert!(audio.volume_of(&firefox).unwrap() < 0.8, "and so must the damage");

        // --- the next start ---
        let mut next = ducker(&store, &audio, instant());
        let rec = next.recover_owned().unwrap();
        assert!(rec.found);
        assert!(!rec.corrupt);
        assert!(!rec.skipped_live_owner);
        assert_eq!(rec.restore.restored, 2);
        assert_eq!(rec.restore.respected, 0);
        assert_eq!(rec.restore.vanished, 0);
        assert_eq!(rec.restore.failed, 0);

        assert_eq!(audio.volumes(), before, "every volume is exactly where the operator left it");
        assert!(!store.journal().exists(), "a paid debt is deleted");

        // And a second recovery is a clean no-op rather than a second restore.
        assert!(next.recover_owned().unwrap().clean());
        assert_eq!(audio.volumes(), before);
    }

    #[test]
    fn a_crash_halfway_through_the_fade_still_restores_the_original() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.9);
        let key = StreamKey::new(1, "Firefox");
        let cfg = DuckConfig { fade_ms: 200, ..Default::default() };

        let mut d = ducker(&store, &audio, cfg.clone());
        d.duck(0);
        d.tick(80); // mid-ramp
        let mid = audio.volume_of(&key).unwrap();
        assert!(mid < 0.9 && mid > cfg.target_for(0.9), "genuinely mid-ramp: {mid}");
        std::mem::forget(d);

        let mut next = ducker(&store, &audio, cfg);
        let rec = next.recover_owned().unwrap();
        assert_eq!(
            rec.restore.restored, 1,
            "the journal has to track the ramp, not just its destination"
        );
        assert_eq!(audio.volume_of(&key).unwrap(), 0.9);
    }

    #[test]
    fn a_recovery_also_respects_a_volume_the_operator_moved_after_she_died() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let key = StreamKey::new(1, "Firefox");

        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        std::mem::forget(d);

        // She died with it ducked; then they turned it up themselves.
        audio.operator_sets(&key, 1.0);
        let mut next = ducker(&store, &audio, instant());
        let rec = next.recover_owned().unwrap();
        assert_eq!(rec.restore.respected, 1);
        assert_eq!(audio.volume_of(&key).unwrap(), 1.0);
        assert!(!store.journal().exists(), "respected or not, the debt is settled");
    }

    #[test]
    fn a_recovery_skips_streams_that_are_no_longer_playing() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let key = StreamKey::new(1, "Firefox");
        let mut d = ducker(&store, &audio, instant());
        d.duck(0);
        std::mem::forget(d);

        audio.remove(&key); // they closed the tab while she was dead
        let mut next = ducker(&store, &audio, instant());
        let rec = next.recover_owned().unwrap();
        assert_eq!(rec.restore.vanished, 1);
        assert_eq!(rec.restore.failed, 0);
        assert!(!store.journal().exists());
    }

    #[test]
    fn a_journal_from_another_live_instance_is_left_alone_rather_than_replayed() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.2);
        let key = StreamKey::new(1, "Firefox");
        // pid 1 exists on every Linux box and is not us. Its recorded start
        // time is the real one, so it reads as genuinely alive.
        let j = Journal {
            version: JOURNAL_VERSION,
            pid: 1,
            pid_start: proc_start_ticks(1),
            boot_id: current_boot_id(),
            started_at: 0,
            entries: vec![JournalEntry { key: key.clone(), original: 0.9, ducked: 0.2, set: 0.2 }],
        };
        write_journal_to(&store.journal(), &j).unwrap();

        let mut d = ducker(&store, &audio, instant());
        let rec = d.recover_owned().unwrap();
        assert!(rec.found);
        assert!(rec.skipped_live_owner, "a second instance is holding those streams on purpose");
        assert_eq!(rec.restore, Restored::default());
        assert_eq!(audio.volume_of(&key).unwrap(), 0.2, "fighting it would make her stutter");
        assert!(
            store.journal().exists(),
            "and its journal is the only thing that restores those volumes if IT is killed"
        );
    }

    #[test]
    fn a_journal_whose_pid_was_recycled_by_someone_else_is_still_replayed() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.2);
        let key = StreamKey::new(1, "Firefox");
        let j = Journal {
            version: JOURNAL_VERSION,
            pid: 1,
            // A start time that cannot match whatever pid 1 really is.
            pid_start: Some(u64::MAX),
            boot_id: current_boot_id(),
            started_at: 0,
            entries: vec![JournalEntry { key: key.clone(), original: 0.9, ducked: 0.2, set: 0.2 }],
        };
        write_journal_to(&store.journal(), &j).unwrap();

        let mut d = ducker(&store, &audio, instant());
        let rec = d.recover_owned().unwrap();
        assert!(!rec.skipped_live_owner, "that pid belongs to a different process now");
        assert_eq!(rec.restore.restored, 1);
        assert_eq!(audio.volume_of(&key).unwrap(), 0.9);
    }

    #[test]
    fn a_journal_from_a_previous_boot_is_cleared_without_touching_anything() {
        let store = TempStore::new();
        let audio = FakeMixer::new();
        let j = Journal {
            version: JOURNAL_VERSION,
            pid: 1,
            pid_start: proc_start_ticks(1),
            boot_id: Some("00000000-0000-0000-0000-000000000000".into()),
            started_at: 0,
            entries: vec![JournalEntry {
                key: StreamKey::new(1, "Firefox"),
                original: 0.9,
                ducked: 0.2,
                set: 0.2,
            }],
        };
        write_journal_to(&store.journal(), &j).unwrap();

        let mut d = ducker(&store, &audio, instant());
        let rec = d.recover_owned().unwrap();
        assert!(rec.found);
        assert!(!rec.skipped_live_owner, "no pid from a dead boot is still alive");
        assert_eq!(rec.restore.vanished, 1, "and none of its serials exist any more");
        assert!(!store.journal().exists());
    }

    #[test]
    fn a_truncated_journal_does_not_stop_her_starting() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.3);
        std::fs::write(store.journal(), br#"{"version":1,"pid":42,"entr"#).unwrap();
        let mut d = ducker(&store, &audio, instant());
        let rec = d.recover_owned().unwrap();
        assert!(rec.found);
        assert!(rec.corrupt);
        assert!(!store.journal().exists(), "a broken repair tool is thrown away, not kept");
        assert_eq!(audio.volume_of(&StreamKey::new(1, "Firefox")).unwrap(), 0.3);
    }

    #[test]
    fn a_journal_from_a_future_version_is_treated_as_unreadable() {
        let store = TempStore::new();
        let audio = FakeMixer::new();
        std::fs::write(
            store.journal(),
            br#"{"version":99,"pid":1,"pid_start":null,"boot_id":null,"started_at":0,"entries":[]}"#,
        )
        .unwrap();
        let mut d = ducker(&store, &audio, instant());
        assert!(d.recover_owned().unwrap().corrupt);
        assert!(!store.journal().exists());
    }

    #[test]
    fn no_journal_at_all_is_the_free_common_case() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let mut d = ducker(&store, &audio, instant());
        assert!(d.recover_owned().unwrap().clean());
        assert_eq!(audio.lists(), 0, "a clean start must not even ask the audio server");
    }

    // -- Drop ---------------------------------------------------------------

    #[test]
    fn dropping_the_ducker_without_releasing_still_restores() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let key = StreamKey::new(1, "Firefox");
        {
            let mut d = ducker(&store, &audio, instant());
            d.duck(0);
            assert!(audio.volume_of(&key).unwrap() < 0.8);
            // No release: an early return, or an unwinding panic.
        }
        assert_eq!(audio.volume_of(&key).unwrap(), 0.8);
        assert!(!store.journal().exists(), "Drop settles the debt and clears the record");
    }

    #[test]
    fn dropping_a_ducker_that_never_ducked_touches_nothing() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        drop(ducker(&store, &audio, instant()));
        assert_eq!(audio.sets(), 0);
        assert!(!store.journal().exists());
    }

    #[test]
    fn a_panic_between_duck_and_release_still_gives_the_music_back() {
        let store = TempStore::new();
        let audio = FakeMixer::new().playing("Firefox", 1, 0.8);
        let key = StreamKey::new(1, "Firefox");
        let dir = store.path().to_path_buf();

        let a = audio.clone();
        let r = std::panic::catch_unwind(move || {
            let mut d = Ducker::new_in(
                &dir,
                Box::new(a.clone()),
                DuckConfig { fade_ms: 0, ..Default::default() },
            );
            d.duck(0);
            assert!(a.volume_of(&StreamKey::new(1, "Firefox")).unwrap() < 0.8);
            panic!("the synthesiser fell over mid-sentence");
        });
        assert!(r.is_err());
        assert_eq!(audio.volume_of(&key).unwrap(), 0.8);
        assert!(!store.journal().exists(), "the unwind ran Drop, which paid the debt");
    }

    // -- the pactl parser, against real captured output ----------------------

    /// Verbatim `pactl -f json list sink-inputs` from the machine this was
    /// written on (`pactl 17.0-98-gb096`, `pipewire-pulse`), trimmed to three
    /// of the five streams and reflowed, with two channel volumes made
    /// asymmetric so the balance case is covered. Nothing else about the shape
    /// is altered — in particular `object.serial` really is a **string**, the
    /// mute field really is spelled `mute`, and `volume` really is an object
    /// keyed by channel name rather than an array.
    const REAL_PACTL_JSON: &str = r#"[
  {
    "index": 354, "driver": "PipeWire", "owner_module": null, "client": "345",
    "sink": 135, "sample_specification": "s16le 2ch 48000Hz",
    "channel_map": "front-left,front-right",
    "corked": false, "mute": false,
    "volume": {
      "front-left":  { "value": 65536, "value_percent": "100%", "db": "0.00 dB" },
      "front-right": { "value": 65536, "value_percent": "100%", "db": "0.00 dB" }
    },
    "balance": 0.00, "resample_method": "PipeWire",
    "properties": {
      "client.api": "pipewire-pulse",
      "application.name": "WEBRTC VoiceEngine",
      "application.process.id": "3990",
      "application.process.binary": "Discord",
      "media.name": "playStream",
      "media.class": "Stream/Output/Audio",
      "node.name": "WEBRTC VoiceEngine",
      "object.id": "200",
      "object.serial": "354"
    }
  },
  {
    "index": 2126, "driver": "PipeWire", "owner_module": null, "client": "2125",
    "sink": 135, "sample_specification": "float32le 2ch 48000Hz",
    "channel_map": "front-left,front-right",
    "corked": true, "mute": true,
    "volume": {
      "front-left":  { "value": 39977, "value_percent": "61%", "db": "-12.88 dB" },
      "front-right": { "value": 19988, "value_percent": "30%", "db": "-18.90 dB" }
    },
    "balance": 0.00, "resample_method": "PipeWire",
    "properties": {
      "application.name": "THE FINALS",
      "application.process.binary": "wine64-preloader",
      "media.name": "audio stream #1",
      "node.name": "THE FINALS",
      "object.id": "302",
      "object.serial": "2126"
    }
  },
  {
    "index": 3148, "driver": "PipeWire", "owner_module": null, "client": "580",
    "sink": 135, "sample_specification": "float32le 2ch 44100Hz",
    "channel_map": "front-left,front-right",
    "corked": false, "mute": false,
    "volume": {
      "front-left":  { "value": 49152, "value_percent": "75%", "db": "-7.50 dB" },
      "front-right": { "value": 49152, "value_percent": "75%", "db": "-7.50 dB" }
    },
    "balance": 0.00, "resample_method": "PipeWire",
    "properties": {
      "application.name": "Firefox",
      "application.process.binary": "firefox",
      "media.name": "Tracks - TIDAL",
      "node.name": "Firefox",
      "object.id": "340",
      "object.serial": "3148"
    }
  }
]"#;

    #[test]
    fn real_pactl_output_parses_into_the_streams_it_describes() {
        let got = parse_sink_inputs(REAL_PACTL_JSON).unwrap();
        assert_eq!(got.len(), 3);

        let ff = &got[2];
        assert_eq!(ff.index, 3148);
        assert_eq!(ff.serial, 3148);
        assert_eq!(ff.app, "Firefox");
        assert_eq!(ff.media_name, "Tracks - TIDAL");
        assert!(!ff.muted && !ff.corked);
        assert_eq!(ff.channels.len(), 2);
        assert!((ff.volume() - 0.75).abs() < 1e-3, "{}", ff.volume());
        assert_eq!(ff.key(), StreamKey::new(3148, "Firefox"));
    }

    #[test]
    fn the_scalar_volume_of_an_unbalanced_stream_is_its_loudest_channel() {
        let got = parse_sink_inputs(REAL_PACTL_JSON).unwrap();
        let finals = &got[1];
        assert!((finals.channels[0] - 0.61).abs() < 1e-3, "{:?}", finals.channels);
        assert!((finals.channels[1] - 0.305).abs() < 1e-3, "{:?}", finals.channels);
        assert!(
            (finals.volume() - 0.61).abs() < 1e-3,
            "the mean would report a panned stream as quieter than it sounds"
        );
    }

    #[test]
    fn mute_and_cork_are_read_from_the_fields_pactl_actually_uses() {
        let got = parse_sink_inputs(REAL_PACTL_JSON).unwrap();
        assert!(got[1].muted, "the field is `mute`, not `muted`");
        assert!(got[1].corked);
        assert!(!got[0].muted);
    }

    #[test]
    fn channel_order_comes_from_the_channel_map_and_not_from_the_json_object() {
        // serde_json stores an object in a BTreeMap, so `volume` arrives
        // alphabetically. For 5.1 that is not the channel order, and writing it
        // back positionally would swap the centre and front-left channels.
        let json = r#"[{
          "index": 7, "channel_map": "front-left,front-right,front-center,lfe,rear-left,rear-right",
          "mute": false, "corked": false,
          "volume": {
            "front-left":   { "value": 65536 },
            "front-right":  { "value": 58982 },
            "front-center": { "value": 52429 },
            "lfe":          { "value": 45875 },
            "rear-left":    { "value": 39322 },
            "rear-right":   { "value": 32768 }
          },
          "properties": { "application.name": "mpv", "object.serial": "7" }
        }]"#;
        let s = &parse_sink_inputs(json).unwrap()[0];
        let pct: Vec<u32> = s.channels.iter().map(|c| (c * 100.0).round() as u32).collect();
        assert_eq!(pct, vec![100, 90, 80, 70, 60, 50], "channel_map order, not alphabetical");
    }

    #[test]
    fn a_mono_stream_has_exactly_one_channel() {
        let json = r#"[{
          "index": 1, "channel_map": "mono", "mute": false, "corked": false,
          "volume": { "mono": { "value": 32768, "value_percent": "50%", "db": "-6.02 dB" } },
          "properties": { "application.name": "aplay", "object.serial": "1" }
        }]"#;
        let s = &parse_sink_inputs(json).unwrap()[0];
        assert_eq!(s.channels.len(), 1);
        assert!((s.volume() - 0.5).abs() < 1e-3);
    }

    #[test]
    fn a_stream_with_no_serial_falls_back_to_its_index_and_leans_on_the_app_name() {
        let json = r#"[{
          "index": 42, "channel_map": "mono", "mute": false, "corked": false,
          "volume": { "mono": { "value": 65536 } },
          "properties": { "application.name": "legacy-pulse-app" }
        }]"#;
        let s = &parse_sink_inputs(json).unwrap()[0];
        assert_eq!(s.serial, 42);
        assert_eq!(s.key(), StreamKey::new(42, "legacy-pulse-app"));
    }

    #[test]
    fn a_stream_with_no_application_name_still_gets_a_usable_identity() {
        let json = r#"[{
          "index": 9, "channel_map": "mono", "mute": false, "corked": false,
          "volume": { "mono": { "value": 65536 } },
          "properties": { "node.name": "alsa_playback.speaker-test", "object.serial": "9" }
        }]"#;
        let s = &parse_sink_inputs(json).unwrap()[0];
        assert_eq!(s.app, "alsa_playback.speaker-test");
    }

    #[test]
    fn a_channel_map_that_does_not_match_the_volume_map_is_flattened_not_guessed() {
        let json = r#"[{
          "index": 3, "channel_map": "front-left,front-right,front-center",
          "mute": false, "corked": false,
          "volume": { "aux0": { "value": 65536 }, "aux1": { "value": 32768 } },
          "properties": { "application.name": "odd", "object.serial": "3" }
        }]"#;
        let s = &parse_sink_inputs(json).unwrap()[0];
        assert_eq!(s.channels, vec![1.0, 1.0], "the loudest channel, applied uniformly");
    }

    #[test]
    fn a_volume_above_a_hundred_percent_is_read_as_the_boost_it_is() {
        let json = r#"[{
          "index": 5, "channel_map": "mono", "mute": false, "corked": false,
          "volume": { "mono": { "value": 98304, "value_percent": "150%", "db": "3.52 dB" } },
          "properties": { "application.name": "loud", "object.serial": "5" }
        }]"#;
        let s = &parse_sink_inputs(json).unwrap()[0];
        assert!((s.volume() - 1.5).abs() < 1e-3, "{}", s.volume());
    }

    #[test]
    fn one_unparseable_entry_does_not_cost_her_the_rest_of_the_list() {
        let json = r#"[
          { "index": 1, "channel_map": "mono", "volume": { "mono": { "value": 65536 } },
            "properties": { "application.name": "good", "object.serial": "1" } },
          { "index": 2, "properties": { "application.name": "no volume at all" } },
          "not even an object",
          { "index": 3, "channel_map": "mono", "volume": { "mono": { "value": 32768 } },
            "properties": { "application.name": "also good", "object.serial": "3" } }
        ]"#;
        let got = parse_sink_inputs(json).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].app, "good");
        assert_eq!(got[1].app, "also good");
    }

    #[test]
    fn an_empty_list_is_a_normal_answer_and_not_an_error() {
        assert!(parse_sink_inputs("[]").unwrap().is_empty());
        assert!(parse_sink_inputs("[]\n").unwrap().is_empty());
    }

    #[test]
    fn pactl_output_that_is_not_a_list_is_an_error_rather_than_a_panic() {
        for bad in ["", "Connection failure: Connection refused", "{}", "null", "["] {
            assert!(parse_sink_inputs(bad).is_err(), "{bad:?} must not parse as a stream list");
        }
    }

    /// The manual check. Reads real volumes and prints what it *would* do, and
    /// is physically unable to change one: [`PactlMixer::probe`] is a dry run.
    /// Ignored so an ordinary `cargo test` on a machine somebody is using never
    /// reaches the audio server at all.
    #[test]
    #[ignore = "talks to the real audio server; run with --ignored on purpose"]
    fn manual_the_real_backend_parses_this_machines_streams() {
        let mut m = PactlMixer::probe();
        let streams = m.playback_streams().expect("pactl");
        let cfg = DuckConfig::default();
        for s in &streams {
            eprintln!(
                "{:<28} {:>5.2}  muted={}  would duck to {:.2}",
                s.app,
                s.volume,
                s.muted,
                cfg.target_for(s.volume)
            );
        }
        // A dry-run set exercises the whole write path down to argv without
        // touching anything.
        if let Some(s) = streams.first() {
            m.set_volume(&s.key, cfg.target_for(s.volume)).unwrap();
        }
    }
}
