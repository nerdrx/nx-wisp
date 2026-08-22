//! Turning text into a vector.
//!
//! Two implementations, and the choice between them is a graceful degradation
//! rather than a fork in the code:
//!
//! * [`ModelEmbedder`] runs the registry's embedding model
//!   (`Qwen3-Embedding-0.6B`) through a [`crate::backend::Backend`]. This is
//!   what ships.
//! * [`HashEmbedder`] hashes character n-grams and words into a fixed-width
//!   vector. No model, no GPU, no download — and, importantly, *not a stub*:
//!   overlapping wording really does produce a higher cosine, so recall degrades
//!   to something lexical instead of to nothing. It is what runs at T3, on a
//!   first boot before the model has been fetched, and in CI.
//!
//! Both produce L2-normalised vectors, so cosine similarity is a dot product
//! and [`crate::memory::index`] never has to divide.

use crate::backend::ModelHandle;
use crate::error::Result;
use crate::manager::{lock_backend, SharedBackend};

pub trait Embedder: Send {
    fn dim(&self) -> usize;
    /// A stable identifier for *which* embedder produced a vector. Mixing two
    /// embedders' vectors in one index would give silently nonsensical recall,
    /// so the store records this and refuses to compare across it.
    fn id(&self) -> String;
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(&[text.to_string()])?;
        Ok(v.pop().unwrap_or_else(|| vec![0.0; self.dim()]))
    }
}

/// The real one.
///
/// Owns a handle to the shared backend rather than borrowing it, so it can live
/// inside the memory store and inside an async tool closure without dragging a
/// lifetime through half the crate.
pub struct ModelEmbedder {
    backend: SharedBackend,
    handle: ModelHandle,
    dim: usize,
    model: String,
}

impl ModelEmbedder {
    pub fn new(
        backend: SharedBackend,
        handle: ModelHandle,
        dim: usize,
        model: impl Into<String>,
    ) -> Self {
        ModelEmbedder {
            backend,
            handle,
            dim,
            model: model.into(),
        }
    }
}

impl Embedder for ModelEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn id(&self) -> String {
        format!("model:{}:{}", self.model, self.dim)
    }
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = lock_backend(&self.backend).embed(self.handle, texts)?;
        for v in &mut out {
            normalise(v);
        }
        Ok(out)
    }
}

/// The fallback, and the one CI uses.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        HashEmbedder::new(256)
    }
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        assert!(dim >= 8, "an embedding narrower than 8 is not an embedding");
        HashEmbedder { dim }
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn id(&self) -> String {
        format!("hash:v1:{}", self.dim)
    }
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_embed(t, self.dim)).collect())
    }
}

/// Words plus character trigrams, hashed into `dim` buckets and L2-normalised.
///
/// Words carry most of the weight; trigrams exist so that "compiling" and
/// "compiled" are not strangers. Signed hashing (the sign bit of a second hash
/// decides whether a feature adds or subtracts) keeps unrelated features from
/// piling up in the same direction, which is what makes a 256-wide vector
/// behave at all.
pub fn hash_embed(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    let lower = text.to_lowercase();

    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        add(&mut v, dim, &format!("w:{word}"), 1.0);
        let chars: Vec<char> = word.chars().collect();
        for w in chars.windows(3) {
            let tri: String = w.iter().collect();
            add(&mut v, dim, &format!("t:{tri}"), 0.35);
        }
    }
    normalise(&mut v);
    v
}

fn add(v: &mut [f32], dim: usize, feature: &str, weight: f32) {
    let h = fnv1a(feature.as_bytes());
    let idx = (h % dim as u64) as usize;
    let sign = if (fnv1a_seeded(feature.as_bytes(), 0x9e37_79b9_7f4a_7c15) & 1) == 0 {
        1.0
    } else {
        -1.0
    };
    v[idx] += weight * sign;
}

fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_seeded(bytes, 0xcbf2_9ce4_8422_2325)
}

fn fnv1a_seeded(bytes: &[u8], seed: u64) -> u64 {
    let mut h = seed;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn normalise(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity of two already-normalised vectors, which is just their dot
/// product. Mismatched widths score zero rather than panicking: an index that
/// has outlived an embedder change should degrade, not crash.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_wording_scores_higher_than_unrelated_wording() {
        let a = hash_embed("the shader cache is recompiling again", 256);
        let b = hash_embed("shaders are recompiling", 256);
        let c = hash_embed("your coffee is getting cold", 256);
        assert!(
            cosine(&a, &b) > cosine(&a, &c),
            "{} vs {}",
            cosine(&a, &b),
            cosine(&a, &c)
        );
    }

    #[test]
    fn embeddings_are_unit_length_and_deterministic() {
        let a = hash_embed("nothing leaves this machine", 128);
        let b = hash_embed("nothing leaves this machine", 128);
        assert_eq!(a, b);
        let n: f32 = a.iter().map(|x| x * x).sum();
        assert!((n - 1.0).abs() < 1e-5, "norm was {n}");
    }

    #[test]
    fn an_empty_string_does_not_produce_nan() {
        let v = hash_embed("", 64);
        assert!(v.iter().all(|x| x.is_finite()));
        assert_eq!(cosine(&v, &v), 0.0);
    }
}
