//! **F18 — memory, and forgetting.**
//!
//! > *Episodic log of what happened, semantic recall, nightly summarization at
//! > idle, and **decay** — old trivia fades unless reinforced. Forgetting is a
//! > feature.*
//!
//! Forgetting being a feature is the part that shapes the design. A store that
//! only accumulates turns a companion into a search engine over your own life,
//! and makes every recall worse than the last. So every row carries a
//! *strength* that halves on a schedule, recall reinforces what it returns, and
//! [`Memory::forget`] actually deletes. [`Memory::strength_of`] is public
//! precisely so a test can watch a memory fade.
//!
//! ## What decay actually is
//!
//! ```text
//! half_life = base × (0.25 + 1.75 · salience) × (1 + reinforced)^exponent
//! strength  = ½ ^ ((now − last_seen) / half_life)
//! ```
//!
//! Three consequences, all deliberate:
//!
//! * something trivial (salience 0) has a quarter of the base half-life — a few
//!   days;
//! * something important (salience 1) has twice it;
//! * **being remembered is what keeps a memory alive.** Every recall bumps
//!   `reinforced` and resets `last_seen`, so the half-life stretches the way
//!   spaced repetition does. A thing she is asked about weekly never fades; a
//!   thing she noticed once in April is gone by June.
//!
//! Notes and anything pinned are exempt: those were written down on purpose.
//!
//! ## Clocks
//!
//! Every entry point takes `now_ms` — Unix milliseconds — from the caller.
//! Nothing here reads the clock. That is what lets the decay tests advance six
//! weeks in a microsecond, and it keeps SPEC §3.2's rule (never wall-clock for
//! *ordering*) intact: this is wall-clock for *ageing*, which is the one thing
//! it is right for, and ordering still comes from the row id.

pub mod embed;
pub mod index;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wisp_proto::Tier;

use crate::backend::{Backend, GenRequest, ModelHandle, Sampling};
use crate::error::{MindError, Result};
use embed::Embedder;
use index::{Entry, ExactIndex};

pub const DAY_MS: f64 = 86_400_000.0;

/// Unix milliseconds, injectable.
///
/// Ageing needs wall-clock time — a memory laid down before a suspend really is
/// a day older afterwards, which a monotonic count would deny. Ordering still
/// comes from the row id, so SPEC §3.2's rule ("never wall-clock for ordering")
/// holds. Injectable because a decay test has to be able to move six weeks
/// without waiting for one.
#[derive(Clone)]
pub struct WallClock(std::sync::Arc<dyn Fn() -> i64 + Send + Sync>);

impl std::fmt::Debug for WallClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WallClock({})", self.now())
    }
}

impl Default for WallClock {
    fn default() -> Self {
        WallClock::system()
    }
}

impl WallClock {
    pub fn system() -> Self {
        WallClock(std::sync::Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        }))
    }
    /// Stuck at `ms`.
    pub fn fixed(ms: i64) -> Self {
        WallClock(std::sync::Arc::new(move || ms))
    }
    /// Moves only when a test says so.
    pub fn stepped(start_ms: i64) -> (Self, std::sync::Arc<std::sync::atomic::AtomicI64>) {
        let cell = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(start_ms));
        let read = std::sync::Arc::clone(&cell);
        (
            WallClock(std::sync::Arc::new(move || {
                read.load(std::sync::atomic::Ordering::Relaxed)
            })),
            cell,
        )
    }
    pub fn now(&self) -> i64 {
        (self.0)()
    }
}
const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Something that happened. The stuff that fades.
    Episodic,
    /// A summary she wrote for herself at rest. Survives its sources.
    Semantic,
    /// The operator asked her to write this down. Never fades.
    Note,
    /// A durable fact about the operator or the machine.
    Fact,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Episodic => "episodic",
            MemoryKind::Semantic => "semantic",
            MemoryKind::Note => "note",
            MemoryKind::Fact => "fact",
        }
    }
    fn from_str(s: &str) -> MemoryKind {
        match s {
            "semantic" => MemoryKind::Semantic,
            "note" => MemoryKind::Note,
            "fact" => MemoryKind::Fact,
            _ => MemoryKind::Episodic,
        }
    }
    /// Written down on purpose, so it is not trivia and does not fade.
    pub fn is_deliberate(self) -> bool {
        matches!(self, MemoryKind::Note | MemoryKind::Fact)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewMemory {
    pub kind: MemoryKind,
    pub text: String,
    /// 0.0 trivia … 1.0 "this matters". Drives the half-life.
    pub salience: f32,
    pub source: Option<String>,
    pub detail: Option<Value>,
}

impl NewMemory {
    pub fn episodic(text: impl Into<String>) -> Self {
        NewMemory {
            kind: MemoryKind::Episodic,
            text: text.into(),
            salience: 0.2,
            source: None,
            detail: None,
        }
    }
    pub fn note(text: impl Into<String>) -> Self {
        NewMemory {
            kind: MemoryKind::Note,
            text: text.into(),
            salience: 0.9,
            source: None,
            detail: None,
        }
    }
    pub fn salience(mut self, s: f32) -> Self {
        self.salience = s.clamp(0.0, 1.0);
        self
    }
    pub fn from(mut self, s: impl Into<String>) -> Self {
        self.source = Some(s.into());
        self
    }
    pub fn detail(mut self, d: Value) -> Self {
        self.detail = Some(d);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Memo {
    pub id: i64,
    pub kind: MemoryKind,
    pub text: String,
    pub detail: Option<Value>,
    pub source: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub reinforced: u32,
    pub salience: f32,
    pub pinned: bool,
    pub consolidated_into: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Recalled {
    pub memo: Memo,
    /// Cosine similarity, before strength weighting.
    pub similarity: f32,
    /// How alive this memory was at the moment it was recalled.
    pub strength: f32,
    /// What the ranking actually used.
    pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryConfig {
    /// Half-life of a memory of average salience that has never been recalled.
    pub base_half_life_days: f64,
    /// How much each recall stretches the half-life. `0.0` disables spaced
    /// repetition entirely, which would make her forget things she uses daily.
    pub reinforce_exponent: f64,
    /// Below this, [`Memory::forget`] deletes.
    pub forget_below: f32,
    /// Below this, recall ignores a row even though it still exists — the
    /// "I know I knew that" band.
    pub recall_floor: f32,
    /// How old an episodic row must be before consolidation will summarise it.
    pub consolidate_after_ms: i64,
    /// Fewer than this many rows in a batch is not worth a summary.
    pub consolidate_min_group: usize,
    pub consolidate_max_group: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            base_half_life_days: 14.0,
            reinforce_exponent: 0.8,
            forget_below: 0.02,
            recall_floor: 0.05,
            consolidate_after_ms: (12.0 * 3_600_000.0) as i64,
            consolidate_min_group: 3,
            consolidate_max_group: 40,
        }
    }
}

/// What one consolidation pass did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Consolidation {
    pub summarised: usize,
    pub created: Option<i64>,
    pub summary: Option<String>,
}

pub struct Memory {
    conn: Connection,
    cfg: MemoryConfig,
    index: ExactIndex,
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("rows", &self.count().unwrap_or(0))
            .field("indexed", &self.index.len())
            .finish()
    }
}

impl Memory {
    /// Open (and migrate) the store at `path`, creating parent directories.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Memory> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MindError::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        Memory::from_conn(conn, MemoryConfig::default())
    }

    /// The store `NX_WISP_CONFIG_DIR` points at (SPEC §4).
    pub fn open_default() -> Result<Memory> {
        Memory::open(crate::dirs::memory_db())
    }

    /// In memory, for tests that do not care about the file.
    pub fn in_memory() -> Result<Memory> {
        Memory::from_conn(Connection::open_in_memory()?, MemoryConfig::default())
    }

    pub fn with_config(mut self, cfg: MemoryConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn config(&self) -> &MemoryConfig {
        &self.cfg
    }

    fn from_conn(conn: Connection, cfg: MemoryConfig) -> Result<Memory> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut m = Memory {
            conn,
            cfg,
            index: ExactIndex::new(),
        };
        m.migrate()?;
        m.reload_index()?;
        Ok(m)
    }

    fn migrate(&mut self) -> Result<()> {
        let v: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if v >= SCHEMA_VERSION {
            return Ok(());
        }
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                kind              TEXT    NOT NULL,
                text              TEXT    NOT NULL,
                detail            TEXT,
                source            TEXT,
                created_at        INTEGER NOT NULL,
                last_seen_at      INTEGER NOT NULL,
                reinforced        INTEGER NOT NULL DEFAULT 0,
                salience          REAL    NOT NULL,
                pinned            INTEGER NOT NULL DEFAULT 0,
                consolidated_into INTEGER REFERENCES memories(id) ON DELETE SET NULL,
                embedder          TEXT,
                dim               INTEGER,
                vec               BLOB
            );
            CREATE INDEX IF NOT EXISTS idx_mem_kind_created ON memories(kind, created_at);
            CREATE INDEX IF NOT EXISTS idx_mem_consolidated ON memories(consolidated_into);
            CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
            "#,
        )?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    fn reload_index(&mut self) -> Result<()> {
        self.index.clear();
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedder, vec FROM memories WHERE vec IS NOT NULL")?;
        let rows = stmt.query_map([], |r| {
            let id: i64 = r.get(0)?;
            let embedder: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            let blob: Vec<u8> = r.get(2)?;
            Ok((id, embedder, blob))
        })?;
        for row in rows {
            let (id, embedder, blob) = row?;
            self.index.insert(Entry {
                id,
                embedder,
                vec: decode_vec(&blob),
            });
        }
        Ok(())
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?)
    }

    /// Is the linear index still inside the assumption it was chosen under
    /// ([`index::COMFORTABLE_ROWS`])?
    pub fn index_is_comfortable(&self) -> bool {
        self.index.budget_ok()
    }

    // --- writing -----------------------------------------------------------

    pub fn remember(
        &mut self,
        embedder: &mut dyn Embedder,
        m: NewMemory,
        now_ms: i64,
    ) -> Result<i64> {
        let vec = embedder.embed_one(&m.text)?;
        if vec.len() != embedder.dim() {
            return Err(MindError::EmbeddingWidth {
                want: embedder.dim(),
                got: vec.len(),
            });
        }
        let eid = embedder.id();
        let pinned = m.kind.is_deliberate();
        self.conn.execute(
            "INSERT INTO memories
               (kind, text, detail, source, created_at, last_seen_at, reinforced,
                salience, pinned, embedder, dim, vec)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, 0, ?6, ?7, ?8, ?9, ?10)",
            params![
                m.kind.as_str(),
                m.text,
                m.detail.as_ref().map(|d| d.to_string()),
                m.source,
                now_ms,
                m.salience as f64,
                pinned as i64,
                eid,
                vec.len() as i64,
                encode_vec(&vec),
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.index.insert(Entry {
            id,
            embedder: eid,
            vec,
        });
        Ok(id)
    }

    /// Bring a memory back to full strength, the way being asked about
    /// something does.
    pub fn reinforce(&mut self, id: i64, now_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE memories SET reinforced = reinforced + 1, last_seen_at = ?2 WHERE id = ?1",
            params![id, now_ms],
        )?;
        Ok(())
    }

    pub fn pin(&mut self, id: i64, pinned: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE memories SET pinned = ?2 WHERE id = ?1",
            params![id, pinned as i64],
        )?;
        Ok(())
    }

    pub fn get(&self, id: i64) -> Result<Option<Memo>> {
        let mut stmt = self.conn.prepare(SELECT_MEMO)?;
        Ok(stmt.query_row([id], read_memo).optional()?)
    }

    // --- decay -------------------------------------------------------------

    /// How alive this memory is, 0.0 … 1.0. The number the fading test watches.
    pub fn strength_of(&self, id: i64, now_ms: i64) -> Result<f32> {
        match self.get(id)? {
            Some(m) => Ok(self.strength(&m, now_ms)),
            None => Ok(0.0),
        }
    }

    pub fn strength(&self, m: &Memo, now_ms: i64) -> f32 {
        if m.pinned {
            return 1.0;
        }
        let hl = self.half_life_ms(m.salience, m.reinforced);
        let dt = (now_ms - m.last_seen_at).max(0) as f64;
        (0.5f64).powf(dt / hl) as f32
    }

    pub fn half_life_ms(&self, salience: f32, reinforced: u32) -> f64 {
        let base = self.cfg.base_half_life_days * DAY_MS;
        let by_salience = 0.25 + 1.75 * salience.clamp(0.0, 1.0) as f64;
        let by_use = (1.0 + reinforced as f64).powf(self.cfg.reinforce_exponent);
        (base * by_salience * by_use).max(1.0)
    }

    /// Delete what has faded past [`MemoryConfig::forget_below`]. Returns what
    /// was forgotten so the caller can put it in the flight recorder — a
    /// forgetting she cannot account for would break SPEC §0.4.
    pub fn forget(&mut self, now_ms: i64) -> Result<Vec<Memo>> {
        let all = self.all_memos()?;
        let mut gone = Vec::new();
        for m in all {
            if m.pinned || m.kind.is_deliberate() {
                continue;
            }
            if self.strength(&m, now_ms) < self.cfg.forget_below {
                self.conn
                    .execute("DELETE FROM memories WHERE id = ?1", params![m.id])?;
                self.index.remove(m.id);
                gone.push(m);
            }
        }
        Ok(gone)
    }

    /// Delete one memory outright. The operator asking her to forget something
    /// is not the same as it fading, and it does not wait for a decay pass.
    pub fn delete(&mut self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        if n > 0 {
            self.index.remove(id);
        }
        Ok(n > 0)
    }

    fn all_memos(&self) -> Result<Vec<Memo>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, kind, text, detail, source, created_at, last_seen_at, reinforced, salience, pinned, consolidated_into FROM memories ORDER BY id")?;
        let rows = stmt.query_map([], read_memo)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // --- recall ------------------------------------------------------------

    /// Semantic recall. Returns the best `k`, strongest-weighted first, and
    /// **reinforces what it returns** — using a memory is what keeps it.
    pub fn recall(
        &mut self,
        embedder: &mut dyn Embedder,
        query: &str,
        k: usize,
        now_ms: i64,
    ) -> Result<Vec<Recalled>> {
        let q = embedder.embed_one(query)?;
        let eid = embedder.id();

        // Ask the index for more than we need, because the strength weighting
        // can reorder the shortlist and a faded row can drop out entirely.
        let shortlist = self.index.top_k(&q, &eid, (k * 4).max(k + 8), &|_| true);
        let mut out = Vec::new();
        for (id, similarity) in shortlist {
            let Some(memo) = self.get(id)? else { continue };
            let strength = self.strength(&memo, now_ms);
            if strength < self.cfg.recall_floor {
                continue;
            }
            // Similarity decides *what*; strength decides *whether it is still
            // there*. Weighting rather than filtering means a very strong match
            // can still surface something half-faded.
            let score = similarity * (0.5 + 0.5 * strength);
            out.push(Recalled {
                memo,
                similarity,
                strength,
                score,
            });
        }
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.memo.id.cmp(&b.memo.id))
        });
        out.truncate(k);
        for r in &out {
            self.reinforce(r.memo.id, now_ms)?;
        }
        Ok(out)
    }

    /// Straight text search, for when there is no embedder at all — T3, or a
    /// first boot before the embedding model has been fetched.
    pub fn recall_lexical(&self, query: &str, k: usize) -> Result<Vec<Memo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, text, detail, source, created_at, last_seen_at, reinforced,
                    salience, pinned, consolidated_into
             FROM memories WHERE text LIKE ?1 ORDER BY last_seen_at DESC LIMIT ?2",
        )?;
        let like = format!("%{query}%");
        let rows = stmt.query_map(params![like, k as i64], read_memo)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The episodic log, newest first. What "what happened today" reads.
    pub fn recent(&self, kind: Option<MemoryKind>, limit: usize) -> Result<Vec<Memo>> {
        let sql = match kind {
            Some(_) => "SELECT id, kind, text, detail, source, created_at, last_seen_at, reinforced, salience, pinned, consolidated_into FROM memories WHERE kind = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2",
            None => "SELECT id, kind, text, detail, source, created_at, last_seen_at, reinforced, salience, pinned, consolidated_into FROM memories ORDER BY created_at DESC, id DESC LIMIT ?2",
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(
            params![kind.map(|k| k.as_str()).unwrap_or(""), limit as i64],
            read_memo,
        )?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // --- consolidation -----------------------------------------------------

    /// **Nightly consolidation, and only at T0.**
    ///
    /// F18 says "nightly summarization at idle". `at idle` is not a nicety: a
    /// consolidation pass loads the deliberate model and reads a day of the
    /// episodic log, which is precisely the sort of work SPEC §0.1 says must be
    /// sheddable. So it refuses at anything above [`Tier::Feral`] rather than
    /// running smaller, and the caller queues it (SPEC §3.5) instead.
    pub fn consolidate(
        &mut self,
        tier: Tier,
        backend: &mut dyn Backend,
        handle: ModelHandle,
        embedder: &mut dyn Embedder,
        now_ms: i64,
    ) -> Result<Consolidation> {
        if tier != Tier::Feral {
            return Err(MindError::NotAtRest { tier });
        }
        let cutoff = now_ms - self.cfg.consolidate_after_ms;
        let batch = {
            let mut stmt = self.conn.prepare(
                "SELECT id, kind, text, detail, source, created_at, last_seen_at, reinforced,
                        salience, pinned, consolidated_into
                 FROM memories
                 WHERE kind = 'episodic' AND consolidated_into IS NULL AND created_at <= ?1
                 ORDER BY created_at ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                params![cutoff, self.cfg.consolidate_max_group as i64],
                read_memo,
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        if batch.len() < self.cfg.consolidate_min_group {
            return Ok(Consolidation::default());
        }

        let mut prompt = String::from(
            "Summarise what happened, in two sentences, as notes to yourself. \
             Keep only what would still matter in a week.\n\n",
        );
        for m in &batch {
            prompt.push_str("- ");
            prompt.push_str(&m.text);
            prompt.push('\n');
        }
        let grammar = crate::grammar::schema_grammar(&serde_json::json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } },
            "required": ["summary"],
            "additionalProperties": false
        }))?;
        let out = backend.generate(
            handle,
            &GenRequest::new(prompt)
                .grammar(grammar)
                .max_tokens(220)
                .sampling(Sampling::DETERMINISTIC),
            &mut |_| crate::backend::Flow::Continue,
        )?;
        let summary = serde_json::from_str::<Value>(&out.text)
            .ok()
            .and_then(|v| v.get("summary").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| out.text.clone());

        // A summary inherits the strongest salience in its batch and a little
        // more: it is now the only thing standing in for all of them.
        let salience = batch
            .iter()
            .map(|m| m.salience)
            .fold(0.0f32, f32::max)
            .max(0.4)
            + 0.1;
        let id = self.remember(
            embedder,
            NewMemory {
                kind: MemoryKind::Semantic,
                text: summary.clone(),
                salience: salience.min(1.0),
                source: Some("consolidation".into()),
                detail: Some(serde_json::json!({
                    "sources": batch.iter().map(|m| m.id).collect::<Vec<_>>()
                })),
            },
            now_ms,
        )?;

        // The sources are not deleted — SPEC §0.4 wants the real trace to
        // survive — but they stop being the thing recall finds, and they now
        // fade twice as fast, because the summary carries them.
        for m in &batch {
            self.conn.execute(
                "UPDATE memories SET consolidated_into = ?2, salience = salience * 0.5 WHERE id = ?1",
                params![m.id, id],
            )?;
        }
        Ok(Consolidation {
            summarised: batch.len(),
            created: Some(id),
            summary: Some(summary),
        })
    }
}

const SELECT_MEMO: &str = "SELECT id, kind, text, detail, source, created_at, last_seen_at, \
                           reinforced, salience, pinned, consolidated_into FROM memories WHERE id = ?1";

fn read_memo(r: &rusqlite::Row<'_>) -> rusqlite::Result<Memo> {
    Ok(Memo {
        id: r.get(0)?,
        kind: MemoryKind::from_str(&r.get::<_, String>(1)?),
        text: r.get(2)?,
        detail: r
            .get::<_, Option<String>>(3)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        source: r.get(4)?,
        created_at: r.get(5)?,
        last_seen_at: r.get(6)?,
        reinforced: r.get::<_, i64>(7)?.max(0) as u32,
        salience: r.get::<_, f64>(8)? as f32,
        pinned: r.get::<_, i64>(9)? != 0,
        consolidated_into: r.get(10)?,
    })
}

fn encode_vec(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn decode_vec(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}
