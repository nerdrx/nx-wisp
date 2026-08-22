//! **F20 — the flight recorder.**
//!
//! SPEC §0.4: *"The flight recorder holds the real trace. 'Why did you say
//! that?' is answerable from data, not from a plausible story she makes up."*
//! SPEC §3.2: *"Every event is recorded by the flight recorder before
//! dispatch."*
//!
//! Those two sentences are the whole design brief, and they rule out the
//! obvious implementations:
//!
//! * **Not a subscriber on the bus.** A subscriber records *after* dispatch, so
//!   a crash between the two loses exactly the event that explains the crash.
//!   Instead the senses publish onto an inner channel, and the only bridge to
//!   the bus everything else reads goes through [`Recorder::record`]
//!   ([`crate::app`] owns that relay). Recording is on the critical path by
//!   construction, not by convention.
//! * **Not a summary.** Nothing is aggregated, sampled or filtered on the way
//!   in. `explain` reads the same bytes the operator can `cat`.
//! * **Not `fsync` per event.** At T3 the whole process is budgeted at ~0.5% of
//!   one core, so a synchronous flush per event would be the single most
//!   expensive thing she does. The file is buffered and flushed every
//!   `flush_every` records, on every read, and at shutdown. A hard kill loses
//!   at most a fraction of a second of trace and never corrupts the file:
//!   JSONL's unit of atomicity is a line.
//!
//! # Time
//!
//! `at` is monotonic milliseconds since *this run* started, per SPEC §3.2 —
//! a suspend or an NTP step must not be able to reorder the trace. That makes
//! `at` useless for ordering *across* runs, so every record also carries:
//!
//! * `seq`, a counter that continues across restarts and rotations. This is the
//!   ordering key.
//! * `session`, the wall-clock epoch-millisecond stamp of the run that produced
//!   it. It is an identity, and because it is also an origin, wall-clock
//!   display time is just `session + at`. No second file, no clock skew.
//!
//! # Rotation
//!
//! `flight.jsonl` is the live file; `flight.1.jsonl` … `flight.N.jsonl` are the
//! rotated generations, `.1` being the most recent. Total on-disk cost is
//! bounded by `max_bytes * (keep + 1)`, which is what makes it safe to leave
//! running forever on the operator's machine.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use wisp_proto::{Event, EventKind, Millis, Observation, Tier, TierReason};

use crate::config::RecorderPrefs;
use crate::fmt;

/// One line of the JSONL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Ordering key. Continues across restarts and rotations.
    pub seq: u64,
    /// Which run produced this, and the wall-clock origin of that run.
    pub session: u64,
    /// Monotonic milliseconds since that run started.
    pub at: Millis,
    pub kind: EventKind,
}

impl Record {
    /// Wall-clock epoch milliseconds, for display only. Never for ordering.
    pub fn wall_ms(&self) -> u64 {
        self.session.saturating_add(self.at)
    }

    pub fn tag(&self) -> &'static str {
        fmt::event_tag(&self.kind)
    }

    /// `+00:01:12.340  sensed    focus moved to org.kde.kate — lib.rs`
    pub fn line(&self) -> String {
        format!("{}  {:<9} {}", fmt::since_start(self.at), self.tag(), fmt::event(&self.kind))
    }

    /// The same, with local wall-clock time instead of the run offset.
    pub fn line_wall(&self) -> String {
        format!(
            "{}  {:<9} {}",
            fmt::wall_clock(self.wall_ms()),
            self.tag(),
            fmt::event(&self.kind)
        )
    }
}

/// What `wisp log --kind` accepts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KindFilter {
    #[default]
    All,
    /// Any of these tags. `wisp log --kind said,dropped`.
    Tags(Vec<String>),
}

/// Every tag [`KindFilter`] understands, in the order `--help` lists them.
pub const TAGS: &[&str] = &[
    "sensed", "tier", "proposed", "said", "dropped", "tool", "deferred", "replayed", "model",
    "invasive",
];

impl KindFilter {
    pub fn parse(s: &str) -> Result<KindFilter, String> {
        let mut tags = Vec::new();
        for raw in s.split(',') {
            let t = raw.trim().to_ascii_lowercase();
            if t.is_empty() {
                continue;
            }
            if t == "all" {
                return Ok(KindFilter::All);
            }
            if !TAGS.contains(&t.as_str()) {
                return Err(format!(
                    "There is no event kind called {t}. The kinds are: {}.",
                    TAGS.join(", ")
                ));
            }
            tags.push(t);
        }
        if tags.is_empty() {
            Ok(KindFilter::All)
        } else {
            Ok(KindFilter::Tags(tags))
        }
    }

    pub fn matches(&self, r: &Record) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::Tags(tags) => tags.iter().any(|t| t == r.tag()),
        }
    }
}

// ---------------------------------------------------------------------------
// File names
// ---------------------------------------------------------------------------

pub fn live_path(dir: &Path) -> PathBuf {
    dir.join(crate::config::FLIGHT_FILE)
}

fn rotated_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(format!("flight.{n}.jsonl"))
}

/// Every generation that exists, oldest first, with the live file last. This is
/// chronological order, which is the order a reader wants.
pub fn generations(dir: &Path, keep: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for n in (1..=keep.max(1)).rev() {
        let p = rotated_path(dir, n);
        if p.exists() {
            out.push(p);
        }
    }
    let live = live_path(dir);
    if live.exists() {
        out.push(live);
    }
    out
}

// ---------------------------------------------------------------------------
// The recorder
// ---------------------------------------------------------------------------

struct Inner {
    file: BufWriter<File>,
    bytes: u64,
    seq: u64,
    unflushed: u32,
    /// The tail of this run, so `explain` and `log` in a running process do not
    /// have to touch the disk at all.
    ring: VecDeque<Record>,
}

pub struct Recorder {
    dir: PathBuf,
    prefs: RecorderPrefs,
    session: u64,
    inner: Mutex<Inner>,
}

/// How much of the current run is kept in memory. Enough that `explain` never
/// reads a file in the running process, small enough to be free at T3.
const RING: usize = 512;

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("dir", &self.dir)
            .field("session", &self.session)
            .field("seq", &self.next_seq())
            .finish()
    }
}

impl Recorder {
    /// Open (or create) the recorder in `dir`.
    ///
    /// The sequence counter continues from whatever is already on disk, so a
    /// restart does not produce two records claiming to be number one.
    pub fn open(dir: &Path, prefs: RecorderPrefs, session: u64) -> std::io::Result<Recorder> {
        std::fs::create_dir_all(dir)?;
        let path = live_path(dir);
        let seq = last_seq(dir, prefs.keep).map(|s| s + 1).unwrap_or(0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Recorder {
            dir: dir.to_path_buf(),
            prefs,
            session,
            inner: Mutex::new(Inner {
                file: BufWriter::with_capacity(16 * 1024, file),
                bytes,
                seq,
                unflushed: 0,
                ring: VecDeque::with_capacity(RING),
            }),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    pub fn prefs(&self) -> RecorderPrefs {
        self.prefs
    }

    pub fn next_seq(&self) -> u64 {
        self.lock().seq
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic inside the recorder must not wedge the event loop: recovering
        // a poisoned lock loses at most the record that panicked.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record one event. **Call this before dispatching it.**
    ///
    /// Returns the sequence number it was given. Never fails: an I/O error is
    /// logged and the in-memory ring still gets the record, because losing the
    /// trace is bad but losing the event loop is worse.
    pub fn record(&self, event: &Event) -> u64 {
        let mut g = self.lock();
        let seq = g.seq;
        g.seq += 1;
        let rec = Record { seq, session: self.session, at: event.at, kind: event.kind.clone() };

        match serde_json::to_vec(&rec) {
            Ok(mut line) => {
                line.push(b'\n');
                if g.bytes + line.len() as u64 > self.prefs.max_bytes {
                    if let Err(e) = self.rotate(&mut g) {
                        tracing::warn!(error = %e, "flight recorder could not rotate");
                    }
                }
                if let Err(e) = g.file.write_all(&line) {
                    tracing::warn!(error = %e, "flight recorder write failed");
                } else {
                    g.bytes += line.len() as u64;
                }
                g.unflushed += 1;
                if g.unflushed >= self.prefs.flush_every {
                    let _ = g.file.flush();
                    g.unflushed = 0;
                }
            }
            Err(e) => tracing::warn!(error = %e, "an event would not serialise"),
        }

        if g.ring.len() == RING {
            g.ring.pop_front();
        }
        g.ring.push_back(rec);
        seq
    }

    /// Convenience: record a `kind` stamped at `at`.
    pub fn record_kind(&self, at: Millis, kind: EventKind) -> u64 {
        self.record(&Event { at, kind })
    }

    pub fn flush(&self) {
        let mut g = self.lock();
        let _ = g.file.flush();
        g.unflushed = 0;
    }

    /// The tail of the **current run**, without touching the disk.
    pub fn ring(&self) -> Vec<Record> {
        self.lock().ring.iter().cloned().collect()
    }

    /// Last `n` records from disk, oldest first, across every generation.
    pub fn tail(&self, n: usize) -> Vec<Record> {
        self.flush();
        tail_from(&self.dir, self.prefs.keep, n)
    }

    /// Last `n` records matching `filter`, oldest first.
    pub fn filtered(&self, filter: &KindFilter, n: usize) -> Vec<Record> {
        self.flush();
        filtered_from(&self.dir, self.prefs.keep, filter, n)
    }

    /// Why she said the last thing she said.
    pub fn explain_last(&self) -> Option<Explanation> {
        self.flush();
        // A generous read: `explain` is interactive, so a few thousand lines is
        // cheap, and a long quiet spell between the observation and the
        // utterance must not fall off the end of the window.
        let records = tail_from(&self.dir, self.prefs.keep, 8_000);
        explain(&records, self.prefs.explain_window_ms)
    }

    /// Rename `flight.jsonl` out of the way and start a fresh one.
    fn rotate(&self, g: &mut Inner) -> std::io::Result<()> {
        g.file.flush()?;
        let keep = self.prefs.keep;
        if keep == 0 {
            // No history wanted: truncate rather than growing forever.
            let f = OpenOptions::new().create(true).truncate(true).write(true).open(live_path(&self.dir))?;
            g.file = BufWriter::with_capacity(16 * 1024, f);
            g.bytes = 0;
            return Ok(());
        }
        // Drop the oldest, shuffle the rest down, then move the live file to .1.
        let _ = std::fs::remove_file(rotated_path(&self.dir, keep));
        for n in (1..keep).rev() {
            let from = rotated_path(&self.dir, n);
            if from.exists() {
                let _ = std::fs::rename(&from, rotated_path(&self.dir, n + 1));
            }
        }
        std::fs::rename(live_path(&self.dir), rotated_path(&self.dir, 1))?;
        let f = OpenOptions::new().create(true).append(true).open(live_path(&self.dir))?;
        g.file = BufWriter::with_capacity(16 * 1024, f);
        g.bytes = 0;
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.flush();
    }
}

// ---------------------------------------------------------------------------
// Reading, as free functions so the CLI can query a recorder it did not open
// ---------------------------------------------------------------------------

/// Read every generation, oldest first. A malformed line is skipped rather than
/// aborting the read: a torn last line from a hard kill must not cost the
/// operator the whole trace.
pub fn read_all(dir: &Path, keep: u32) -> Vec<Record> {
    let mut out = Vec::new();
    for path in generations(dir, keep) {
        read_into(&path, &mut out);
    }
    out
}

fn read_into(path: &Path, out: &mut Vec<Record>) {
    let Ok(f) = File::open(path) else { return };
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(&line) {
            Ok(r) => out.push(r),
            Err(_) => continue,
        }
    }
}

pub fn tail_from(dir: &Path, keep: u32, n: usize) -> Vec<Record> {
    let mut all = read_all(dir, keep);
    if all.len() > n {
        all.drain(..all.len() - n);
    }
    all
}

pub fn filtered_from(dir: &Path, keep: u32, filter: &KindFilter, n: usize) -> Vec<Record> {
    let mut hits: Vec<Record> =
        read_all(dir, keep).into_iter().filter(|r| filter.matches(r)).collect();
    if hits.len() > n {
        hits.drain(..hits.len() - n);
    }
    hits
}

/// The highest `seq` on disk, so a restart continues the count.
fn last_seq(dir: &Path, keep: u32) -> Option<u64> {
    read_all(dir, keep).last().map(|r| r.seq)
}

/// Total bytes the recorder is occupying.
pub fn disk_bytes(dir: &Path, keep: u32) -> u64 {
    generations(dir, keep)
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

// ---------------------------------------------------------------------------
// "Why did you say that?"
// ---------------------------------------------------------------------------

/// The walk-back from a `Said` to the events that produced it.
///
/// Every field is a record that is really in the file. Nothing here is
/// inferred, reconstructed or paraphrased — if the trace does not contain the
/// answer, the field is empty and [`Explanation::render`] says so. That is the
/// difference between SPEC §0.4 and a plausible story.
#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    /// The `Said`.
    pub said: Record,
    /// The `Proposed` that carried the same text, if it is still in the trace.
    pub proposed: Option<Record>,
    /// The tier in force when she said it — the last `TierChanged` at or before.
    pub tier: Option<Record>,
    /// The `Sensed` records inside the window before the proposal, newest last.
    pub triggers: Vec<Record>,
    /// Tools that ran between the proposal and the utterance.
    pub tools: Vec<Record>,
    /// Thoughts dropped in the same window — what she chose *not* to say.
    pub also_dropped: Vec<Record>,
    /// How long she held it before saying it.
    pub held_ms: Option<Millis>,
    pub window_ms: Millis,
}

/// How many observations are worth showing. More than this and it is a log,
/// not an explanation.
const MAX_TRIGGERS: usize = 8;

/// Pure. Takes records in file order and walks back from the last `Said`.
///
/// Being a free function over a slice is deliberate: the interesting cases
/// (nothing said, a proposal that rotated off the end, an alarm that jumped the
/// queue) are all testable without a filesystem.
pub fn explain(records: &[Record], window_ms: Millis) -> Option<Explanation> {
    let said_i = records.iter().rposition(|r| matches!(r.kind, EventKind::Said { .. }))?;
    let said = records[said_i].clone();
    let EventKind::Said { text: ref said_text } = said.kind else { unreachable!() };

    // The proposal that became this utterance: same text, same run, before it.
    let proposed_i = records[..said_i].iter().rposition(|r| {
        r.session == said.session
            && matches!(&r.kind, EventKind::Proposed(u) if &u.text == said_text)
    });
    let proposed = proposed_i.map(|i| records[i].clone());

    // The tier she was in when she said it. Not restricted to the run: if
    // nothing has changed tier since a restart, the last known change is still
    // the honest answer.
    let tier = records[..=said_i]
        .iter()
        .rposition(|r| matches!(r.kind, EventKind::TierChanged { .. }))
        .map(|i| records[i].clone());

    // The window runs back from the proposal, or from the utterance when the
    // proposal is no longer in the trace.
    let anchor_i = proposed_i.unwrap_or(said_i);
    let anchor = &records[anchor_i];
    let from = anchor.at.saturating_sub(window_ms);

    let mut triggers: Vec<Record> = records[..=anchor_i]
        .iter()
        .rev()
        .take_while(|r| r.session == anchor.session && r.at >= from)
        .filter(|r| matches!(r.kind, EventKind::Sensed(_)))
        .take(MAX_TRIGGERS)
        .cloned()
        .collect();
    triggers.reverse();

    let tools: Vec<Record> = records[anchor_i..=said_i]
        .iter()
        .filter(|r| matches!(r.kind, EventKind::ToolCall { .. }))
        .cloned()
        .collect();

    let also_dropped: Vec<Record> = records[..=said_i]
        .iter()
        .rev()
        .take_while(|r| r.session == said.session && r.at >= from)
        .filter(|r| matches!(r.kind, EventKind::Dropped { .. }))
        .take(MAX_TRIGGERS)
        .cloned()
        .collect();

    let held_ms = proposed.as_ref().map(|p| said.at.saturating_sub(p.at));

    Some(Explanation { said, proposed, tier, triggers, tools, also_dropped, held_ms, window_ms })
}

impl Explanation {
    pub fn text(&self) -> &str {
        match &self.said.kind {
            EventKind::Said { text } => text,
            _ => "",
        }
    }

    pub fn tier_at(&self) -> Option<(Tier, TierReason)> {
        match self.tier.as_ref()?.kind.clone() {
            EventKind::TierChanged { to, reason, .. } => Some((to, reason)),
            _ => None,
        }
    }

    /// Plain text, in DESIGN.md §9's voice.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("She said: {}\n", fmt::quote(self.text())));
        s.push_str(&format!(
            "  at {} into the run (record {})\n",
            fmt::since_start(self.said.at),
            self.said.seq
        ));
        match self.tier_at() {
            Some((tier, r)) => s.push_str(&format!(
                "  tier {} — {}\n",
                fmt::tier_label(tier),
                fmt::reason(&r)
            )),
            None => s.push_str("  tier unknown; no tier change is in the trace\n"),
        }

        s.push('\n');
        match (&self.proposed, self.held_ms) {
            (Some(p), Some(held)) => {
                let urgency = match &p.kind {
                    EventKind::Proposed(u) => fmt::urgency_word(u.urgency),
                    _ => "unknown",
                };
                if held == 0 {
                    s.push_str(&format!("It was proposed as a {urgency} and said at once.\n"));
                } else {
                    s.push_str(&format!(
                        "It was proposed as a {urgency} and held {} before she said it.\n",
                        fmt::duration(held)
                    ));
                }
            }
            _ => s.push_str(
                "The proposal is no longer in the trace, so the window below is measured \
                 back from the utterance itself.\n",
            ),
        }

        s.push('\n');
        if self.triggers.is_empty() {
            s.push_str(&format!(
                "She had seen nothing in the {} before it. This was not a reaction to \
                 anything the senses reported.\n",
                fmt::duration(self.window_ms)
            ));
        } else {
            s.push_str(&format!(
                "What she had seen in the {} before:\n",
                fmt::duration(self.window_ms)
            ));
            for t in &self.triggers {
                s.push_str(&format!("  {}  {}\n", fmt::since_start(t.at), fmt::event(&t.kind)));
            }
        }

        if !self.tools.is_empty() {
            s.push_str("\nTools that ran first:\n");
            for t in &self.tools {
                s.push_str(&format!("  {}  {}\n", fmt::since_start(t.at), fmt::event(&t.kind)));
            }
        }

        if !self.also_dropped.is_empty() {
            s.push_str("\nWhat she decided not to say in the same window:\n");
            for d in &self.also_dropped {
                s.push_str(&format!("  {}  {}\n", fmt::since_start(d.at), fmt::event(&d.kind)));
            }
        }

        s
    }
}

/// The `Sensed` observations in an explanation, for callers that want the data
/// rather than the prose.
pub fn observations(records: &[Record]) -> Vec<&Observation> {
    records
        .iter()
        .filter_map(|r| match &r.kind {
            EventKind::Sensed(o) => Some(o),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;
    use wisp_proto::{SenseId, Urgency, Utterance};

    fn prefs() -> RecorderPrefs {
        RecorderPrefs::default()
    }

    fn ev(at: Millis, kind: EventKind) -> Event {
        Event { at, kind }
    }

    fn sensed(at: Millis, app: &str) -> Event {
        ev(
            at,
            EventKind::Sensed(Observation::Focus {
                app_id: app.to_string(),
                title: "t".into(),
            }),
        )
    }

    #[test]
    fn records_land_on_disk_in_order_and_read_back_whole() {
        let tmp = TempConfig::new();
        let r = Recorder::open(tmp.path(), prefs(), 1_700_000_000_000).unwrap();
        for i in 0..50u64 {
            r.record(&sensed(i * 100, &format!("app{i}")));
        }
        let back = r.tail(1_000);
        assert_eq!(back.len(), 50);
        for (i, rec) in back.iter().enumerate() {
            assert_eq!(rec.seq, i as u64, "seq must be dense and ordered");
            assert_eq!(rec.at, i as u64 * 100);
            assert_eq!(rec.session, 1_700_000_000_000);
        }
        assert_eq!(back.last().unwrap().wall_ms(), 1_700_000_000_000 + 4_900);
    }

    #[test]
    fn a_restart_continues_the_sequence_rather_than_starting_again() {
        let tmp = TempConfig::new();
        {
            let r = Recorder::open(tmp.path(), prefs(), 1_000).unwrap();
            for i in 0..10 {
                r.record(&sensed(i, "a"));
            }
        }
        let r2 = Recorder::open(tmp.path(), prefs(), 2_000).unwrap();
        assert_eq!(r2.next_seq(), 10);
        r2.record(&sensed(0, "b"));
        let all = r2.tail(100);
        assert_eq!(all.len(), 11);
        assert_eq!(all[10].seq, 10);
        assert_eq!(all[10].session, 2_000, "the new run is distinguishable");
        // `at` restarted at zero, which is exactly why `seq` is the order key.
        assert!(all[10].at < all[9].at);
        assert!(all[10].seq > all[9].seq);
    }

    #[test]
    fn rotation_caps_the_total_and_keeps_the_newest() {
        let tmp = TempConfig::new();
        let p = RecorderPrefs { max_bytes: 8 * 1024, keep: 2, ..prefs() };
        let r = Recorder::open(tmp.path(), p, 1).unwrap();
        for i in 0..4_000u64 {
            r.record(&sensed(i, "some.application.with.a.longish.id"));
        }
        r.flush();

        let on_disk = disk_bytes(tmp.path(), p.keep);
        let ceiling = p.max_bytes * (p.keep as u64 + 1);
        assert!(on_disk <= ceiling, "{on_disk} bytes on disk, cap is {ceiling}");
        assert!(on_disk > p.max_bytes, "rotation kept nothing at all");

        // Whatever survived must be the newest records, contiguous.
        let all = read_all(tmp.path(), p.keep);
        assert!(!all.is_empty());
        assert_eq!(all.last().unwrap().seq, 3_999);
        for w in all.windows(2) {
            assert_eq!(w[1].seq, w[0].seq + 1, "rotation must not interleave generations");
        }
        // And nothing older than `keep` generations is left lying around.
        assert!(!tmp.path().join("flight.3.jsonl").exists());
    }

    #[test]
    fn keep_zero_truncates_instead_of_growing_forever() {
        let tmp = TempConfig::new();
        let p = RecorderPrefs { max_bytes: 4 * 1024, keep: 0, ..prefs() };
        let r = Recorder::open(tmp.path(), p, 1).unwrap();
        for i in 0..2_000u64 {
            r.record(&sensed(i, "app"));
        }
        r.flush();
        assert!(disk_bytes(tmp.path(), 0) <= p.max_bytes);
    }

    #[test]
    fn a_torn_last_line_costs_only_that_line() {
        let tmp = TempConfig::new();
        {
            let r = Recorder::open(tmp.path(), prefs(), 1).unwrap();
            for i in 0..5 {
                r.record(&sensed(i, "a"));
            }
        }
        // Simulate a hard kill mid-write.
        let mut f = OpenOptions::new().append(true).open(live_path(tmp.path())).unwrap();
        f.write_all(br#"{"seq":5,"session":1,"at":9,"kin"#).unwrap();
        drop(f);
        let all = read_all(tmp.path(), 3);
        assert_eq!(all.len(), 5, "the whole trace must survive one torn line");
    }

    #[test]
    fn filters_select_by_kind() {
        let tmp = TempConfig::new();
        let r = Recorder::open(tmp.path(), prefs(), 1).unwrap();
        r.record(&sensed(0, "a"));
        r.record(&ev(1, EventKind::Proposed(Utterance::new("hello there", Urgency::Whim))));
        r.record(&ev(2, EventKind::Said { text: "hello there".into() }));
        r.record(&ev(3, EventKind::Dropped { text: "other".into(), why: "stale".into() }));

        let said = r.filtered(&KindFilter::parse("said").unwrap(), 100);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].tag(), "said");

        let both = r.filtered(&KindFilter::parse("said,dropped").unwrap(), 100);
        assert_eq!(both.len(), 2);

        assert_eq!(r.filtered(&KindFilter::All, 100).len(), 4);
        assert_eq!(r.filtered(&KindFilter::All, 2).len(), 2, "the tail, not the head");

        let err = KindFilter::parse("shouting").unwrap_err();
        assert!(err.contains("sensed"), "the error must list what is valid: {err}");
    }

    #[test]
    fn explain_walks_back_from_the_utterance_to_what_she_saw() {
        let tmp = TempConfig::new();
        let r = Recorder::open(tmp.path(), prefs(), 1).unwrap();
        r.record(&ev(
            1_000,
            EventKind::TierChanged {
                from: Tier::Reduced,
                to: Tier::Full,
                reason: TierReason::Idle,
            },
        ));
        r.record(&sensed(2_000, "org.kde.kate"));
        r.record(&ev(
            2_500,
            EventKind::Sensed(Observation::Idle { idle: true, for_ms: 120_000 }),
        ));
        r.record(&ev(
            3_000,
            EventKind::Proposed(Utterance::new("your branch is behind origin", Urgency::Notable)),
        ));
        r.record(&ev(
            3_100,
            EventKind::Dropped { text: "an idle remark".into(), why: "stale".into() },
        ));
        r.record(&ev(4_500, EventKind::Said { text: "your branch is behind origin".into() }));

        let e = r.explain_last().expect("something was said");
        assert_eq!(e.text(), "your branch is behind origin");
        assert_eq!(e.held_ms, Some(1_500));
        assert_eq!(e.tier_at().map(|(t, _)| t), Some(Tier::Full));
        assert_eq!(e.triggers.len(), 2, "{:?}", e.triggers);
        assert!(matches!(e.triggers[0].kind, EventKind::Sensed(Observation::Focus { .. })));
        assert!(matches!(e.triggers[1].kind, EventKind::Sensed(Observation::Idle { .. })));
        assert_eq!(e.also_dropped.len(), 1);

        let text = e.render();
        assert!(text.contains("your branch is behind origin"), "{text}");
        assert!(text.contains("T1 Full"), "{text}");
        assert!(text.contains("held 1.5 s"), "{text}");
        assert!(text.contains("org.kde.kate"), "{text}");
        assert!(text.contains("decided not to say"), "{text}");
        assert!(!text.contains('!'), "DESIGN.md §9: {text}");
    }

    #[test]
    fn explain_says_so_when_it_does_not_know_rather_than_making_it_up() {
        let tmp = TempConfig::new();
        let r = Recorder::open(tmp.path(), prefs(), 1).unwrap();
        // An utterance with no proposal, no tier change and no observations.
        r.record(&ev(500, EventKind::Said { text: "out of nowhere".into() }));
        let e = r.explain_last().unwrap();
        assert!(e.proposed.is_none());
        assert!(e.tier.is_none());
        assert!(e.triggers.is_empty());
        let text = e.render();
        assert!(text.contains("no tier change is in the trace"), "{text}");
        assert!(text.contains("no longer in the trace"), "{text}");
        assert!(text.contains("not a reaction to anything"), "{text}");
    }

    #[test]
    fn explain_is_none_when_she_has_not_spoken() {
        let tmp = TempConfig::new();
        let r = Recorder::open(tmp.path(), prefs(), 1).unwrap();
        r.record(&sensed(0, "a"));
        assert!(r.explain_last().is_none());
    }

    #[test]
    fn explain_does_not_cross_a_restart_for_the_proposal() {
        // Two runs, the same text. The proposal from the *previous* run must not
        // be presented as the reason for this run's utterance: `at` is
        // per-run, so the interval would be a fiction.
        let recs = vec![
            Record {
                seq: 0,
                session: 1_000,
                at: 500,
                kind: EventKind::Proposed(Utterance::new("the same thought", Urgency::Whim)),
            },
            Record {
                seq: 1,
                session: 2_000,
                at: 100,
                kind: EventKind::Said { text: "the same thought".into() },
            },
        ];
        let e = explain(&recs, 60_000).unwrap();
        assert!(e.proposed.is_none(), "a proposal from another run is not an explanation");
        assert!(e.held_ms.is_none());
    }

    #[test]
    fn the_window_bounds_what_counts_as_a_trigger() {
        let recs = vec![
            Record {
                seq: 0,
                session: 1,
                at: 0,
                kind: EventKind::Sensed(Observation::Idle { idle: true, for_ms: 1 }),
            },
            Record {
                seq: 1,
                session: 1,
                at: 95_000,
                kind: EventKind::Sensed(Observation::Idle { idle: false, for_ms: 0 }),
            },
            Record {
                seq: 2,
                session: 1,
                at: 100_000,
                kind: EventKind::Proposed(Utterance::new("hello", Urgency::Whim)),
            },
            Record { seq: 3, session: 1, at: 100_100, kind: EventKind::Said { text: "hello".into() } },
        ];
        let e = explain(&recs, 10_000).unwrap();
        assert_eq!(e.triggers.len(), 1, "only what is inside the window");
        assert_eq!(e.triggers[0].seq, 1);
    }

    /// SPEC §0.1: at T3 she is budgeted at about half a percent of one core.
    /// The recorder runs on the critical path of every event, so it has to be
    /// nearly free. This bound is deliberately loose — 50 µs per event, when
    /// the real cost is a couple of microseconds — because what it is really
    /// catching is an `fsync` or an `open` creeping into `record`, either of
    /// which costs milliseconds and would blow it by two orders of magnitude.
    #[test]
    fn recording_is_cheap_enough_to_run_at_t3() {
        let tmp = TempConfig::new();
        let r = Recorder::open(tmp.path(), prefs(), 1).unwrap();
        let n = 20_000u64;
        let start = std::time::Instant::now();
        for i in 0..n {
            r.record(&ev(
                i,
                EventKind::Sensed(Observation::Window {
                    id: i,
                    x: 10,
                    y: 20,
                    w: 800,
                    h: 600,
                    gone: false,
                }),
            ));
        }
        r.flush();
        let each = start.elapsed() / n as u32;
        assert!(
            each < std::time::Duration::from_micros(50),
            "{each:?} per event is too expensive for T3"
        );
    }

    #[test]
    fn the_ring_answers_without_touching_the_disk() {
        let tmp = TempConfig::new();
        let p = RecorderPrefs { flush_every: 100_000, ..prefs() };
        let r = Recorder::open(tmp.path(), p, 1).unwrap();
        for i in 0..10 {
            r.record(&sensed(i, "a"));
        }
        // Nothing has been flushed, so the file is still empty…
        assert_eq!(std::fs::metadata(live_path(tmp.path())).unwrap().len(), 0);
        // …and the ring still has the whole run.
        assert_eq!(r.ring().len(), 10);
        // The ring is bounded.
        for i in 0..(RING as u64 * 2) {
            r.record(&sensed(i, "a"));
        }
        assert_eq!(r.ring().len(), RING);
    }

    #[test]
    fn every_event_kind_survives_a_round_trip_through_json() {
        let tmp = TempConfig::new();
        let r = Recorder::open(tmp.path(), prefs(), 1).unwrap();
        let kinds = vec![
            EventKind::Sensed(Observation::Clipboard { len: 3, kind: "text/plain".into() }),
            EventKind::Sensed(Observation::Speech { text: "hi".into(), final_: true }),
            EventKind::TierChanged {
                from: Tier::Full,
                to: Tier::Dormant,
                reason: TierReason::PowerCritical,
            },
            EventKind::Proposed(Utterance {
                text: "a thought".into(),
                urgency: Urgency::Alarm,
                defer_until: Some(5),
                stale_after: Some(10),
                expression: Some("curious".into()),
            }),
            EventKind::Said { text: "a thought".into() },
            EventKind::Dropped { text: "x".into(), why: "y".into() },
            EventKind::ToolCall { name: "nx".into(), args: "{\"a\":1}".into(), ok: false },
            EventKind::Deferred { what: "w".into(), queued: 2 },
            EventKind::Replayed { what: "w".into(), dropped: true },
            EventKind::Model { name: "m".into(), loaded: true, vram_mib: 1 },
            EventKind::InvasiveActive { sense: SenseId::Microphone, active: true },
        ];
        for (i, k) in kinds.iter().enumerate() {
            r.record(&ev(i as u64, k.clone()));
        }
        let back = r.tail(100);
        assert_eq!(back.len(), kinds.len());
        for (rec, want) in back.iter().zip(&kinds) {
            assert_eq!(&rec.kind, want);
        }
    }

    #[test]
    fn generations_are_returned_oldest_first() {
        let tmp = TempConfig::new();
        std::fs::write(tmp.path().join("flight.jsonl"), b"").unwrap();
        std::fs::write(tmp.path().join("flight.1.jsonl"), b"").unwrap();
        std::fs::write(tmp.path().join("flight.2.jsonl"), b"").unwrap();
        let g = generations(tmp.path(), 3);
        let names: Vec<_> =
            g.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["flight.2.jsonl", "flight.1.jsonl", "flight.jsonl"]);
    }
}
