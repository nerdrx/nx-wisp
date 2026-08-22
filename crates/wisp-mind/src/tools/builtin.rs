//! The tools she has without anyone else's help.
//!
//! F16 names them: *timers, notes, file search, window control, media control,
//! launching NX apps, running whitelisted shell recipes, web fetch (opt-in).*
//! The last three already exist elsewhere and are registered rather than
//! rewritten — `wisp-fleet::tools::NxTools` wraps the `nx` CLI, and window
//! control belongs to `wisp-senses`' KWin script. This module owns the four
//! that are genuinely local to cognition: **timers, notes, file search, media
//! control**, plus **recall** into F18's store.
//!
//! Consent (SPEC §3.7) is assigned by what a tool can *do to the world*, not by
//! how alarming it sounds:
//!
//! | Tool | Consent | Why |
//! |---|---|---|
//! | `timer_set` / `timer_list` / `timer_cancel` | Ambient | Her own head. Nothing outside the process changes. |
//! | `note_write` / `recall` | Ambient | Her own memory, on this machine, which she is *for*. |
//! | `memory_forget` | Explicit | Destroys something the operator may have wanted. |
//! | `file_search` | Explicit | Reads the operator's filesystem, even if only names. |
//! | `media_control` | Explicit | Reaches out and changes something the operator can hear. |
//!
//! Two of them take an injected dependency rather than reaching for the system
//! themselves — [`MediaSink`] because MPRIS belongs to `wisp-senses`, and
//! [`FileSearch`] because *which* directories she may look in is the operator's
//! decision and not a default.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wisp_proto::Consent;

use super::{failed, ok, ok_with, sync_tool, unavailable, ToolDescriptor, ToolOutcome};
use crate::memory::{embed::Embedder, Memory, MemoryKind, NewMemory, WallClock};

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timer {
    pub id: u64,
    pub label: String,
    pub due_at_ms: i64,
    pub set_at_ms: i64,
}

/// Timers, with no thread and no clock of their own.
///
/// [`Timers::due`] is polled by the binary's event loop, which already has a
/// clock and already wakes up. A `tokio::time::sleep` per timer would be a
/// second scheduler she does not need and a wakeup source the governor cannot
/// see (SPEC §0.1: everything must be reachable by the governor).
#[derive(Debug, Default)]
pub struct Timers {
    next_id: u64,
    timers: Vec<Timer>,
}

impl Timers {
    pub fn new() -> Self {
        Timers::default()
    }

    pub fn set(&mut self, label: impl Into<String>, in_ms: i64, now_ms: i64) -> Timer {
        self.next_id += 1;
        let t = Timer {
            id: self.next_id,
            label: label.into(),
            due_at_ms: now_ms + in_ms.max(0),
            set_at_ms: now_ms,
        };
        self.timers.push(t.clone());
        t
    }

    pub fn cancel(&mut self, id: u64) -> Option<Timer> {
        let i = self.timers.iter().position(|t| t.id == id)?;
        Some(self.timers.remove(i))
    }

    pub fn list(&self) -> &[Timer] {
        &self.timers
    }

    /// Everything that has come due, removed from the list. Soonest first.
    pub fn due(&mut self, now_ms: i64) -> Vec<Timer> {
        let mut fired: Vec<Timer> = self
            .timers
            .iter()
            .filter(|t| t.due_at_ms <= now_ms)
            .cloned()
            .collect();
        fired.sort_by_key(|t| t.due_at_ms);
        self.timers.retain(|t| t.due_at_ms > now_ms);
        fired
    }

    /// When the next one fires, so the event loop can sleep instead of spin.
    pub fn next_due(&self) -> Option<i64> {
        self.timers.iter().map(|t| t.due_at_ms).min()
    }
}

pub type SharedTimers = Arc<Mutex<Timers>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The three timer tools.
pub fn timer_tools(timers: SharedTimers, clock: WallClock) -> Vec<ToolDescriptor> {
    let set = {
        let timers = Arc::clone(&timers);
        let clock = clock.clone();
        sync_tool(
            "timer_set",
            "Set a timer. Say how long in minutes (or seconds for something \
             short) and what it is for. You will be told when it goes off.",
            Consent::Ambient,
            json!({
                "type": "object",
                "properties": {
                    "label": {"type": "string", "description": "What the timer is for."},
                    "minutes": {"type": "integer"},
                    "seconds": {"type": "integer"}
                },
                "required": ["label"],
                "additionalProperties": false
            }),
            move |args| {
                let label = args
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if label.is_empty() {
                    return failed("a timer needs to be for something.");
                }
                let mins = args.get("minutes").and_then(Value::as_i64).unwrap_or(0);
                let secs = args.get("seconds").and_then(Value::as_i64).unwrap_or(0);
                let total_ms = mins * 60_000 + secs * 1_000;
                if total_ms <= 0 {
                    return failed("how long? I need minutes or seconds.");
                }
                // A year is not a timer, it is a calendar, and she does not
                // have one of those.
                if total_ms > 366 * 86_400_000 {
                    return failed("that is not a timer, that is a diary entry.");
                }
                let t = lock(&timers).set(&label, total_ms, clock.now());
                ok_with(
                    format!("Timer set: {label}, in {}.", human_ms(total_ms)),
                    serde_json::to_value(&t).unwrap_or(Value::Null),
                )
            },
        )
    };

    let list = {
        let timers = Arc::clone(&timers);
        let clock = clock.clone();
        sync_tool(
            "timer_list",
            "List the timers that are still running.",
            Consent::Ambient,
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
            move |_| {
                let now = clock.now();
                let g = lock(&timers);
                let ts = g.list();
                if ts.is_empty() {
                    return ok("No timers running.");
                }
                let words: Vec<String> = ts
                    .iter()
                    .map(|t| format!("{} in {}", t.label, human_ms(t.due_at_ms - now)))
                    .collect();
                ok_with(
                    format!("Running: {}.", words.join(", ")),
                    serde_json::to_value(ts).unwrap_or(Value::Null),
                )
            },
        )
    };

    let cancel = {
        let timers = Arc::clone(&timers);
        sync_tool(
            "timer_cancel",
            "Cancel a timer by its id. Use timer_list first to find the id.",
            Consent::Ambient,
            json!({
                "type": "object",
                "properties": {"id": {"type": "integer"}},
                "required": ["id"],
                "additionalProperties": false
            }),
            move |args| {
                let id = args.get("id").and_then(Value::as_u64).unwrap_or(0);
                match lock(&timers).cancel(id) {
                    Some(t) => ok(format!("Cancelled: {}.", t.label)),
                    None => failed(format!("There is no timer {id}.")),
                }
            },
        )
    };

    vec![set, list, cancel]
}

fn human_ms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0 => "no time at all".to_string(),
        1 => "1 second".to_string(),
        2..=90 => format!("{s} seconds"),
        _ => {
            let m = (s + 30) / 60;
            if m == 1 {
                "1 minute".to_string()
            } else if m < 90 {
                format!("{m} minutes")
            } else {
                format!("{} hours", (m + 30) / 60)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// Memory plus the embedder that wrote it, shareable into an async closure.
///
/// One embedder for the whole store, deliberately: rows carry the id of the
/// embedder that produced them and are never scored against another's, so two
/// embedders writing into one store would quietly split it in half.
#[derive(Clone)]
pub struct MemoryHandle {
    memory: Arc<Mutex<Memory>>,
    embedder: Arc<Mutex<Box<dyn Embedder>>>,
    clock: WallClock,
}

impl std::fmt::Debug for MemoryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryHandle")
            .field("embedder", &lock(&self.embedder).id())
            .finish()
    }
}

impl MemoryHandle {
    pub fn new(
        memory: Arc<Mutex<Memory>>,
        embedder: Box<dyn Embedder>,
        clock: WallClock,
    ) -> Self {
        MemoryHandle {
            memory,
            embedder: Arc::new(Mutex::new(embedder)),
            clock,
        }
    }

    pub fn memory(&self) -> Arc<Mutex<Memory>> {
        Arc::clone(&self.memory)
    }

    pub fn clock(&self) -> &WallClock {
        &self.clock
    }

    /// Swap the embedder — what happens when the embedding model finishes
    /// downloading mid-session. Rows written before the swap keep working via
    /// [`Memory::recall_lexical`]; rows written after are scored against each
    /// other. Nothing is silently mixed.
    pub fn set_embedder(&self, e: Box<dyn Embedder>) -> String {
        let mut g = lock(&self.embedder);
        let was = g.id();
        *g = e;
        was
    }

    pub fn embedder_id(&self) -> String {
        lock(&self.embedder).id()
    }

    /// Write something down.
    pub fn remember(&self, m: NewMemory) -> crate::Result<i64> {
        let now = self.clock.now();
        let mut e = lock(&self.embedder);
        lock(&self.memory).remember(e.as_mut(), m, now)
    }

    /// Look something up. Reinforces what it returns (F18).
    pub fn recall(&self, query: &str, k: usize) -> crate::Result<Vec<crate::memory::Recalled>> {
        let now = self.clock.now();
        let mut e = lock(&self.embedder);
        lock(&self.memory).recall(e.as_mut(), query, k, now)
    }
}

pub fn memory_tools(mem: MemoryHandle) -> Vec<ToolDescriptor> {
    let write = {
        let mem = mem.clone();
        sync_tool(
            "note_write",
            "Write something down so you do not lose it. Notes do not fade the \
             way ordinary memories do, so use this for things the operator \
             asked you to remember.",
            Consent::Ambient,
            json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "about": {"type": "string", "description": "Optional: what this is about."}
                },
                "required": ["text"],
                "additionalProperties": false
            }),
            move |args| {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if text.is_empty() {
                    return failed("there is nothing there to write down.");
                }
                let mut n = NewMemory::note(text.clone());
                if let Some(about) = args.get("about").and_then(Value::as_str) {
                    n = n.from(about);
                }
                match mem.remember(n) {
                    Ok(id) => ok_with(format!("Noted: {text}"), json!({ "id": id })),
                    Err(e) => failed(format!("I could not write that down: {e}")),
                }
            },
        )
    };

    let recall = {
        let mem = mem.clone();
        sync_tool(
            "recall",
            "Search your own memory. Use this before saying you do not know \
             something — you may have seen it before.",
            Consent::Ambient,
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            move |args| {
                let q = args.get("query").and_then(Value::as_str).unwrap_or("");
                if q.trim().is_empty() {
                    return failed("what am I looking for?");
                }
                let k = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(5)
                    .clamp(1, 20) as usize;
                match mem.recall(q, k) {
                    Ok(rs) if rs.is_empty() => ok("Nothing in memory about that."),
                    Ok(rs) => {
                        let lines: Vec<String> = rs.iter().map(|r| r.memo.text.clone()).collect();
                        ok_with(
                            lines.join(" / "),
                            json!({
                                "hits": rs.iter().map(|r| json!({
                                    "id": r.memo.id,
                                    "kind": r.memo.kind.as_str(),
                                    "text": r.memo.text,
                                    // How alive it was. A half-faded memory is
                                    // reported as such rather than presented
                                    // with the confidence of a fresh one.
                                    "strength": r.strength,
                                    "similarity": r.similarity,
                                })).collect::<Vec<_>>()
                            }),
                        )
                    }
                    Err(e) => failed(format!("I could not search my memory: {e}")),
                }
            },
        )
    };

    let forget = {
        let mem = mem.clone();
        sync_tool(
            "memory_forget",
            "Delete one memory by id, permanently. Only when the operator asks.",
            Consent::Explicit,
            json!({
                "type": "object",
                "properties": {"id": {"type": "integer"}},
                "required": ["id"],
                "additionalProperties": false
            }),
            move |args| {
                let id = args.get("id").and_then(Value::as_i64).unwrap_or(-1);
                let m = mem.memory();
                let mut g = lock(&m);
                match g.get(id) {
                    Ok(Some(memo)) => match g.delete(id) {
                        Ok(_) => ok(format!("Forgotten: {}", memo.text)),
                        Err(e) => failed(format!("I could not forget that: {e}")),
                    },
                    Ok(None) => failed(format!("There is no memory {id}.")),
                    Err(e) => failed(format!("I could not look that up: {e}")),
                }
            },
        )
    };

    vec![write, recall, forget]
}

// ---------------------------------------------------------------------------
// File search
// ---------------------------------------------------------------------------

/// Where she is allowed to look, and how hard.
///
/// Empty by default. A file-search tool whose default is "the whole home
/// directory" is a tool that has decided something on the operator's behalf.
#[derive(Debug, Clone)]
pub struct FileSearch {
    pub roots: Vec<PathBuf>,
    pub max_depth: usize,
    pub max_results: usize,
    /// Stop after this many directory entries however few matched, so a search
    /// under a source tree cannot become the reason a frame dropped.
    pub max_visited: usize,
}

impl Default for FileSearch {
    fn default() -> Self {
        FileSearch {
            roots: Vec::new(),
            max_depth: 6,
            max_results: 25,
            max_visited: 20_000,
        }
    }
}

impl FileSearch {
    pub fn under(roots: Vec<PathBuf>) -> Self {
        FileSearch {
            roots,
            ..FileSearch::default()
        }
    }

    /// Names containing `needle`, case-insensitively. Names only — she does not
    /// read the contents, and nothing in this crate ever will without a
    /// separate consent decision.
    pub fn find(&self, needle: &str) -> Vec<PathBuf> {
        let needle = needle.to_lowercase();
        let mut out = Vec::new();
        let mut visited = 0usize;
        for root in &self.roots {
            self.walk(root, 0, &needle, &mut out, &mut visited);
            if out.len() >= self.max_results || visited >= self.max_visited {
                break;
            }
        }
        out
    }

    fn walk(
        &self,
        dir: &Path,
        depth: usize,
        needle: &str,
        out: &mut Vec<PathBuf>,
        visited: &mut usize,
    ) {
        if depth > self.max_depth || out.len() >= self.max_results || *visited >= self.max_visited {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            *visited += 1;
            if *visited >= self.max_visited {
                return;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            // Dotfiles are not hers to rummage through, and following symlinks
            // is how a bounded search stops being bounded.
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if name.to_lowercase().contains(needle) {
                out.push(e.path());
                if out.len() >= self.max_results {
                    return;
                }
            }
            if ft.is_dir() {
                self.walk(&e.path(), depth + 1, needle, out, visited);
            }
        }
    }
}

pub fn file_tools(cfg: Arc<FileSearch>) -> Vec<ToolDescriptor> {
    vec![sync_tool(
        "file_search",
        "Find files and folders by name in the places the operator has let you \
         look. Names only — you cannot read what is in them.",
        Consent::Explicit,
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        move |args| {
            let q = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
            if q.is_empty() {
                return failed("what am I looking for?");
            }
            if cfg.roots.is_empty() {
                return unavailable(
                    "There is nowhere I am allowed to look yet — the operator has not \
                     given me any folders.",
                );
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(cfg.max_results as u64)
                .clamp(1, cfg.max_results as u64) as usize;
            let mut hits = cfg.find(q);
            hits.truncate(limit);
            if hits.is_empty() {
                return ok(format!("Nothing called \"{q}\" where I can see."));
            }
            let names: Vec<String> = hits.iter().map(|p| p.display().to_string()).collect();
            ok_with(
                format!("Found {}: {}", hits.len(), names.join(", ")),
                json!({ "paths": names }),
            )
        },
    )]
}

// ---------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
}

impl MediaAction {
    pub const ALL: [&'static str; 5] = ["play", "pause", "toggle", "next", "previous"];

    fn parse(s: &str) -> Option<MediaAction> {
        Some(match s {
            "play" => MediaAction::Play,
            "pause" => MediaAction::Pause,
            "toggle" => MediaAction::Toggle,
            "next" => MediaAction::Next,
            "previous" => MediaAction::Previous,
            _ => return None,
        })
    }
}

/// MPRIS belongs to `wisp-senses` (SPEC §2), which `wisp-mind` may not depend
/// on. So this is the seam: the binary implements it over the player it already
/// talks to, and cognition never learns what D-Bus is.
pub trait MediaSink: Send + Sync {
    fn control(&self, action: MediaAction) -> std::result::Result<String, String>;
    fn now_playing(&self) -> Option<String> {
        None
    }
}

/// The default. Not an error path: plenty of machines have no player running,
/// and "there is nothing playing" is a perfectly good answer.
pub struct NoMedia;

impl MediaSink for NoMedia {
    fn control(&self, _action: MediaAction) -> std::result::Result<String, String> {
        Err("there is no media player here for me to reach.".to_string())
    }
}

pub fn media_tools(sink: Arc<dyn MediaSink>) -> Vec<ToolDescriptor> {
    vec![sync_tool(
        "media_control",
        "Play, pause or skip whatever is playing.",
        Consent::Explicit,
        json!({
            "type": "object",
            "properties": {
                "action": {"enum": ["play", "pause", "toggle", "next", "previous"]}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        move |args| {
            let Some(action) = args
                .get("action")
                .and_then(Value::as_str)
                .and_then(MediaAction::parse)
            else {
                return failed("I do not know how to do that to a media player.");
            };
            match sink.control(action) {
                Ok(said) => ok(said),
                Err(why) => unavailable(why),
            }
        },
    )]
}

// ---------------------------------------------------------------------------

/// Everything local, wired up. The binary adds `wisp-fleet`'s on top.
pub struct Builtins {
    pub timers: SharedTimers,
    pub memory: MemoryHandle,
    pub files: Arc<FileSearch>,
    pub media: Arc<dyn MediaSink>,
}

impl Builtins {
    pub fn new(memory: MemoryHandle) -> Self {
        Builtins {
            timers: Arc::new(Mutex::new(Timers::new())),
            memory,
            files: Arc::new(FileSearch::default()),
            media: Arc::new(NoMedia),
        }
    }

    pub fn with_files(mut self, files: FileSearch) -> Self {
        self.files = Arc::new(files);
        self
    }

    pub fn with_media(mut self, media: Arc<dyn MediaSink>) -> Self {
        self.media = media;
        self
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        let clock = self.memory.clock().clone();
        let mut v = timer_tools(Arc::clone(&self.timers), clock);
        v.extend(memory_tools(self.memory.clone()));
        v.extend(file_tools(Arc::clone(&self.files)));
        v.extend(media_tools(Arc::clone(&self.media)));
        v
    }

    /// Timers that have come due, for the event loop to turn into utterances.
    pub fn due_timers(&self, now_ms: i64) -> Vec<Timer> {
        lock(&self.timers).due(now_ms)
    }
}

/// A tool result, as something she might propose saying. Still only a
/// *proposal*: SPEC §3.4 gives the decision to `wisp-attn`.
pub fn outcome_to_utterance(o: &ToolOutcome, urgency: wisp_proto::Urgency) -> wisp_proto::Utterance {
    wisp_proto::Utterance::new(o.summary.clone(), urgency)
}

/// Memory kinds a tool is allowed to write. Everything else is hers.
pub fn tool_writable(kind: MemoryKind) -> bool {
    matches!(kind, MemoryKind::Note | MemoryKind::Fact)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers_fire_once_and_in_order() {
        let mut t = Timers::new();
        t.set("tea", 3 * 60_000, 0);
        t.set("stretch", 60_000, 0);
        assert_eq!(t.due(30_000).len(), 0);
        let fired = t.due(120_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].label, "stretch");
        // Not fired twice.
        assert_eq!(t.due(120_000).len(), 0);
        assert_eq!(t.due(10 * 60_000).len(), 1);
        assert!(t.list().is_empty());
    }

    #[test]
    fn the_next_wakeup_is_the_soonest_timer() {
        let mut t = Timers::new();
        assert_eq!(t.next_due(), None);
        t.set("a", 5_000, 1_000);
        t.set("b", 1_000, 1_000);
        assert_eq!(t.next_due(), Some(2_000));
    }

    #[test]
    fn file_search_stays_inside_its_roots_and_ignores_dotfiles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("visible");
        std::fs::create_dir_all(root.join("nested")).expect("mkdir");
        std::fs::write(root.join("shader_cache.txt"), b"x").expect("write");
        std::fs::write(root.join("nested").join("shaders.log"), b"x").expect("write");
        std::fs::write(root.join(".shader_secret"), b"x").expect("write");
        // Outside the root entirely.
        std::fs::write(dir.path().join("shader_outside.txt"), b"x").expect("write");

        let fs = FileSearch::under(vec![root.clone()]);
        let hits = fs.find("shader");
        let names: Vec<String> = hits
            .iter()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"shader_cache.txt".to_string()), "{names:?}");
        assert!(names.contains(&"shaders.log".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with('.')), "{names:?}");
        assert!(
            !names.contains(&"shader_outside.txt".to_string()),
            "search escaped its root: {names:?}"
        );
    }

    #[test]
    fn human_time_reads_like_a_person_said_it() {
        assert_eq!(human_ms(1_000), "1 second");
        assert_eq!(human_ms(45_000), "45 seconds");
        assert_eq!(human_ms(180_000), "3 minutes");
        assert_eq!(human_ms(2 * 3_600_000), "2 hours");
    }
}
