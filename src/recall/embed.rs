//! Local embedding via `fastembed` (all-MiniLM-L6-v2, 384-dim).
//!
//! Implements spec §5.4 (R-130 series) and the embedding half of §9.4
//! (R-532). The embedder is lazily constructed per `Palace` instance
//! and cached thereafter. First use downloads the model (~90 MiB) into
//! `~/.ndx/models/` (R-204).

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::path::PathBuf;

/// Output dimensionality of all-MiniLM-L6-v2. Locked by the model; any
/// change is a breaking schema change per the spec.
pub const EMBEDDING_DIM: usize = 384;

/// Batch size for embedding calls during mining. Chosen to keep peak
/// memory bounded on modest laptops; fastembed internally parallelises.
pub const EMBED_BATCH_SIZE: usize = 32;

/// Human-readable identifier persisted in `META.embedding_model`.
pub const MODEL_ID: &str = "all-MiniLM-L6-v2";

/// Wrapper around fastembed's `TextEmbedding`.
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Load (downloading if necessary) the MiniLM-L6-v2 model. The model
    /// files are cached at `cache_dir`, which should be `~/.ndx/models/`.
    pub fn load(cache_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating {}", cache_dir.display()))?;
        let opts = InitOptions::new(EmbeddingModel::AllMiniLML6V2)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(true);
        let model = TextEmbedding::try_new(opts)
            .context("failed to load fastembed all-MiniLM-L6-v2 model")?;
        Ok(Self { model })
    }

    /// Embed a batch of texts. Returns one 384-dim vector per input.
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let vecs = self
            .model
            .embed(texts, Some(EMBED_BATCH_SIZE))
            .context("embedding failed")?;
        debug_assert!(
            vecs.iter().all(|v| v.len() == EMBEDDING_DIM),
            "fastembed returned wrong-dimensional vector"
        );
        Ok(vecs)
    }

    /// Embed a single text, convenience wrapper.
    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(vec![text.to_string()])?;
        v.pop().context("embedder returned no vectors")
    }
}

/// Encode a 384-dim vector to little-endian f32 bytes for redb storage.
pub fn encode_embedding(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode a redb-stored embedding byte slice back to a vector of f32.
pub fn decode_embedding(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity between two equal-length vectors. Returns 0.0 when
/// either vector has zero magnitude.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let v: Vec<f32> = (0..EMBEDDING_DIM).map(|i| i as f32 / 100.0).collect();
        let bytes = encode_embedding(&v);
        assert_eq!(bytes.len(), EMBEDDING_DIM * 4);
        let decoded = decode_embedding(&bytes);
        assert_eq!(decoded, v);
    }

    #[test]
    fn cosine_basic() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
        let c = vec![0.0, 1.0, 0.0];
        assert!(cosine(&a, &c).abs() < 1e-6);
        let d = vec![0.0, 0.0, 0.0];
        assert_eq!(cosine(&a, &d), 0.0);
    }
}
