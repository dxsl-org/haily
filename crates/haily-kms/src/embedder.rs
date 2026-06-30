/// Embedding generation via fastembed + multilingual-e5-base.
/// Only compiled when `features = ["embeddings"]` is enabled.
///
/// multilingual-e5 requires query/passage prefixes for correct retrieval:
///   stored fact  →  "passage: {text}"
///   search query →  "query: {text}"
use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

pub const EMBEDDING_DIM: usize = 768;

pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Initialize the embedder. Downloads multilingual-e5-base ONNX model on first call.
    /// This blocks — call from `spawn_blocking` in async contexts.
    pub fn init() -> Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::MultilingualE5Base)
                .with_show_download_progress(true),
        )
        .context("init multilingual-e5-base embedder")?;
        Ok(Self { model })
    }

    /// Embed a batch of fact texts (subject + predicate + object).
    /// Adds "passage: " prefix required by multilingual-e5.
    /// Returns one 768-dim embedding per input text.
    pub fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts
            .iter()
            .map(|t| format!("passage: {t}"))
            .collect();
        self.model
            .embed(prefixed, None)
            .context("embed passages")
    }

    /// Embed a single search query.
    /// Adds "query: " prefix required by multilingual-e5.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let prefixed = format!("query: {query}");
        let mut results = self
            .model
            .embed(vec![prefixed], None)
            .context("embed query")?;
        results
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedder returned empty result"))
    }

    /// Convert a `Vec<f32>` embedding to little-endian bytes for SQLite BLOB storage.
    pub fn to_bytes(embedding: &[f32]) -> Vec<u8> {
        embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    /// Recover a `Vec<f32>` from little-endian BLOB bytes.
    pub fn from_bytes(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }
}
