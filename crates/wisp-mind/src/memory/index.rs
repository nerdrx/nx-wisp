//! The vector index, and why it is a linear scan.
//!
//! The plan says "sqlite + HNSW". This is an exact brute-force index instead,
//! and the reason is arithmetic rather than laziness.
//!
//! **One operator, one machine (SPEC §0.5).** The corpus is one person's
//! episodic log. At the rate the senses actually produce memorable events —
//! focus changes worth keeping, a handful of notes, a nightly summary — a busy
//! year is on the order of 10⁴–10⁵ rows. A 1024-wide `f32` dot product over
//! 100 000 rows is 10⁸ multiply-adds: a few tens of milliseconds single-threaded,
//! on a recall path that runs at conversational speed, not per frame. Against
//! that, HNSW costs a graph in memory, an insertion path that is no longer
//! `INSERT INTO`, a rebuild whenever [`super::Memory::forget`] deletes
//! something, a dependency, and *approximate* answers — she would forget things
//! she still knows, which is the one failure mode F18 must not have.
//!
//! So: exact, always right, and honest about its bound. [`ExactIndex::budget_ok`]
//! reports when the corpus has outgrown the assumption, and that — not a hunch —
//! is the trigger to revisit this.
//!
//! Vectors are stored already L2-normalised, so scoring is a dot product with
//! no division in the inner loop.

use std::collections::BinaryHeap;

use super::embed::cosine;

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    /// Which embedder produced this. Vectors from different embedders are not
    /// comparable and are never scored against each other.
    pub embedder: String,
    pub vec: Vec<f32>,
}

/// The size past which the linear scan stops being obviously free. Nothing
/// breaks here; it is the number that says "go and measure".
pub const COMFORTABLE_ROWS: usize = 200_000;

#[derive(Debug, Default, Clone)]
pub struct ExactIndex {
    entries: Vec<Entry>,
}

impl ExactIndex {
    pub fn new() -> Self {
        ExactIndex::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Is the corpus still inside the assumption this index is built on?
    pub fn budget_ok(&self) -> bool {
        self.entries.len() <= COMFORTABLE_ROWS
    }

    pub fn insert(&mut self, e: Entry) {
        match self.entries.iter_mut().find(|x| x.id == e.id) {
            Some(slot) => *slot = e,
            None => self.entries.push(e),
        }
    }

    pub fn remove(&mut self, id: i64) {
        self.entries.retain(|e| e.id != id);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Top `k` by cosine, restricted to one embedder and to whatever the caller
    /// still considers alive.
    pub fn top_k(
        &self,
        query: &[f32],
        embedder: &str,
        k: usize,
        alive: &dyn Fn(i64) -> bool,
    ) -> Vec<(i64, f32)> {
        if k == 0 {
            return Vec::new();
        }
        // A min-heap of the best `k` so far, so memory is O(k) rather than
        // O(corpus) even when the corpus grows past the comfortable bound.
        let mut heap: BinaryHeap<Scored> = BinaryHeap::with_capacity(k + 1);
        for e in &self.entries {
            if e.embedder != embedder || !alive(e.id) {
                continue;
            }
            let s = cosine(query, &e.vec);
            if !s.is_finite() {
                continue;
            }
            heap.push(Scored { score: -s, id: e.id });
            if heap.len() > k {
                heap.pop();
            }
        }
        let mut out: Vec<(i64, f32)> = heap.into_iter().map(|s| (s.id, -s.score)).collect();
        // Ties broken by id so recall is stable run to run — a memory test that
        // depends on hash iteration order is a memory test that flakes.
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        out
    }
}

/// Ordered by score only; `Ord` is total because scores are filtered to finite
/// before they get here.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scored {
    score: f32,
    id: i64,
}

impl Eq for Scored {}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(self.id.cmp(&other.id))
    }
}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::super::embed::hash_embed;
    use super::*;

    fn idx() -> ExactIndex {
        let mut i = ExactIndex::new();
        for (id, text) in [
            (1i64, "the shader cache is recompiling"),
            (2, "coffee is cold"),
            (3, "wivrn started streaming to the headset"),
            (4, "shader compilation finished"),
        ] {
            i.insert(Entry {
                id,
                embedder: "hash:v1:256".into(),
                vec: hash_embed(text, 256),
            });
        }
        i
    }

    #[test]
    fn the_nearest_row_is_the_one_about_the_same_thing() {
        let i = idx();
        let q = hash_embed("shaders recompiling", 256);
        let top = i.top_k(&q, "hash:v1:256", 2, &|_| true);
        assert_eq!(top.len(), 2);
        assert!(top[0].0 == 1 || top[0].0 == 4, "{top:?}");
    }

    #[test]
    fn a_dead_row_is_never_returned_even_if_it_is_the_best_match() {
        let i = idx();
        let q = hash_embed("the shader cache is recompiling", 256);
        let top = i.top_k(&q, "hash:v1:256", 1, &|id| id != 1);
        assert_ne!(top[0].0, 1);
    }

    #[test]
    fn vectors_from_another_embedder_are_invisible() {
        let mut i = idx();
        i.insert(Entry {
            id: 99,
            embedder: "model:qwen3-embed:1024".into(),
            vec: hash_embed("shaders recompiling", 256),
        });
        let q = hash_embed("shaders recompiling", 256);
        let top = i.top_k(&q, "hash:v1:256", 10, &|_| true);
        assert!(top.iter().all(|(id, _)| *id != 99), "{top:?}");
    }
}
