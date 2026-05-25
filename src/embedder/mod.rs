use anyhow::Result;
use std::sync::Arc;

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

pub mod fastembed;

pub fn build_embedder(config: &crate::config::EmbedderConfig) -> Result<Arc<dyn Embedder>> {
    match config.provider.as_str() {
        "fastembed" => Ok(Arc::new(fastembed::FastEmbedEmbedder::new(
            &config.model,
            config.query_prefix.clone().unwrap_or_default(),
            config.doc_prefix.clone().unwrap_or_default(),
        )?)),
        // Test provider: deterministic embeddings, no external dependencies.
        // Not recommended for production use, but harmless if enabled.
        "test" => Ok(Arc::new(TestEmbedder)),
        other => Err(anyhow::anyhow!(
            "unsupported embedder provider '{}'; only 'fastembed' is supported",
            other
        )),
    }
}

/// Deterministic test embedder for unit/integration tests.
/// Does not download models or depend on external services.
#[derive(Default)]
pub struct TestEmbedder;

#[async_trait::async_trait]
impl Embedder for TestEmbedder {
    async fn embed(&self, text: &str, _mode: EmbedMode) -> Result<Vec<f32>> {
        Ok(embedding_for(text))
    }

    async fn embed_batch(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| embedding_for(t)).collect())
    }

    fn dimensions(&self) -> usize {
        768
    }
}

fn embedding_for(text: &str) -> Vec<f32> {
    let lower = text.to_lowercase();
    let mut embedding = vec![0.0; 768];
    if lower.contains("auth") || lower.contains("authentication") || lower.contains("login") {
        embedding[0] = 1.0;
    } else if lower.contains("cache") {
        embedding[1] = 1.0;
    } else {
        embedding[2] = 1.0;
    }
    embedding
}
