use anyhow::Result;

/// Whether the text is a query (user search) or a document (code chunk to index).
/// Some embedding models require different prefixes for each mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedMode {
    Query,
    Document,
}

#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str, mode: EmbedMode) -> Result<Vec<f32>>;
    async fn embed_batch(&self, texts: &[String], mode: EmbedMode) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
}

pub mod ollama;
