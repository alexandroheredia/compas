use super::{EmbedMode, Embedder};
use crate::util::retry;
use anyhow::Result;
use reqwest::Client;
use serde_json::json;

pub struct OllamaEmbedder {
    client: Client,
    base_url: String,
    model: String,
    query_prefix: String,
    doc_prefix: String,
    dims: usize,
}

impl OllamaEmbedder {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        query_prefix: impl Into<String>,
        doc_prefix: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            model: model.into(),
            query_prefix: query_prefix.into(),
            doc_prefix: doc_prefix.into(),
            dims: 768,
        }
    }

    fn prefix_text(&self, text: &str, mode: EmbedMode) -> String {
        match mode {
            EmbedMode::Query => format!("{}{}", self.query_prefix, text),
            EmbedMode::Document => format!("{}{}", self.doc_prefix, text),
        }
    }
}

#[async_trait::async_trait]
impl Embedder for OllamaEmbedder {
    async fn embed(&self, text: &str, mode: EmbedMode) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[text.into()], mode).await?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no embedding returned"))
    }

    async fn embed_batch(&self, texts: &[String], mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts.iter().map(|t| self.prefix_text(t, mode)).collect();

        let payload = json!({
            "model": self.model,
            "input": prefixed,
        });

        let client = self.client.clone();
        let url = format!("{}/api/embed", self.base_url);
        let payload_clone = payload.clone();

        let resp = retry("ollama embed", 3, || {
            let client = client.clone();
            let url = url.clone();
            let payload = payload_clone.clone();
            async move {
                client
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("request failed: {}", e))
            }
        })
        .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "ollama embed returned HTTP {}: {}",
                status,
                body_text
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let embeddings = body["embeddings"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing 'embeddings' field in Ollama response"))?
            .iter()
            .map(|v| {
                v.as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|n| n.as_f64().unwrap_or(0.0) as f32)
                    .collect()
            })
            .collect();

        Ok(embeddings)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}
