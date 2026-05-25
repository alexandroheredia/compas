use super::{EmbedMode, Embedder};
use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::path::PathBuf;
use std::sync::Arc;

pub struct FastEmbedEmbedder {
    model: Arc<std::sync::Mutex<TextEmbedding>>,
    query_prefix: String,
    doc_prefix: String,
    dims: usize,
}

impl FastEmbedEmbedder {
    pub fn new(
        model_name: &str,
        query_prefix: impl Into<String>,
        doc_prefix: impl Into<String>,
    ) -> Result<Self> {
        let model_variant = parse_model(model_name)?;

        // Use a global cache directory so all repos share one model download
        // instead of creating .fastembed_cache in every repo root.
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            .join("compas")
            .join("fastembed");
        std::fs::create_dir_all(&cache_dir).ok();

        let mut model =
            TextEmbedding::try_new(InitOptions::new(model_variant).with_cache_dir(cache_dir))
                .context("failed to initialize FastEmbed model")?;
        let query_prefix = query_prefix.into();
        let doc_prefix = doc_prefix.into();

        // Compute dimensions by embedding a sample string
        let sample = model
            .embed(vec!["sample".to_string()], None)
            .context("failed to compute dimensions from sample embedding")?;
        let dims = sample.first().map(|v| v.len()).unwrap_or(768);

        Ok(Self {
            model: Arc::new(std::sync::Mutex::new(model)),
            query_prefix,
            doc_prefix,
            dims,
        })
    }

    fn prefix_text(&self, text: &str, mode: EmbedMode) -> String {
        match mode {
            EmbedMode::Query => format!("{}{}", self.query_prefix, text),
            EmbedMode::Document => format!("{}{}", self.doc_prefix, text),
        }
    }
}

fn parse_model(model: &str) -> Result<EmbeddingModel> {
    match model {
        "nomic-ai/nomic-embed-text-v1.5" | "nomic-embed-text-v1.5" => {
            Ok(EmbeddingModel::NomicEmbedTextV15)
        }
        other => {
            // Try to match against known variants manually since FromStr may not exist
            let normalized = other.to_lowercase().replace(['-', '_'], "");
            if normalized.contains("nomicembedtextv15") || normalized.contains("nomicembedtextv1.5")
            {
                Ok(EmbeddingModel::NomicEmbedTextV15)
            } else {
                Err(anyhow::anyhow!("unsupported FastEmbed model '{}'", other))
            }
        }
    }
}

#[async_trait::async_trait]
impl Embedder for FastEmbedEmbedder {
    async fn embed(&self, text: &str, mode: EmbedMode) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[text.to_string()], mode).await?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no embedding returned"))
    }

    async fn embed_batch(&self, texts: &[String], mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts
            .iter()
            .map(|text| self.prefix_text(text, mode))
            .collect();
        let model = Arc::clone(&self.model);

        tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| anyhow::anyhow!("fastembed mutex poisoned"))?;
            model
                .embed(prefixed, None)
                .context("failed to generate FastEmbed embeddings")
        })
        .await
        .context("FastEmbed embedding task panicked")?
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}
