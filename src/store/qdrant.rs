use super::Store;
use crate::models::{Chunk, SearchResult};
use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;

pub struct QdrantStore {
    client: Client,
    base_url: String,
    collection: String,
}

impl QdrantStore {
    pub fn new(base_url: impl Into<String>, collection: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            collection: collection.into(),
        }
    }

    /// Check if Qdrant is reachable and the collection exists.
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/collections/{}", self.base_url, self.collection);
        let resp = self.client.get(&url).send().await.map_err(|e| {
            anyhow::anyhow!(
                "cannot reach Qdrant at {}: {}. Is it running?",
                self.base_url,
                e
            )
        })?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "Qdrant collection '{}' not found at {} (HTTP {}). Run `docker-compose up -d`?",
                self.collection,
                self.base_url,
                resp.status()
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Store for QdrantStore {
    async fn init(&self, vector_size: usize) -> Result<()> {
        let url = format!("{}/collections/{}", self.base_url, self.collection);
        let resp = self.client.get(&url).send().await.map_err(|e| {
            anyhow::anyhow!(
                "cannot reach Qdrant at {}: {}. Is it running?",
                self.base_url,
                e
            )
        })?;
        if resp.status().is_success() {
            return Ok(());
        }

        let payload = json!({
            "vectors": {
                "size": vector_size,
                "distance": "Cosine"
            }
        });
        let resp = self
            .client
            .put(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to create Qdrant collection: {}. Is Qdrant reachable?",
                    e
                )
            })?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "Qdrant create collection failed with HTTP {}. Response: {:?}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }
        Ok(())
    }

    async fn upsert(&self, chunks: &[Chunk], embeddings: &[Vec<f32>]) -> Result<()> {
        if chunks.len() != embeddings.len() {
            return Err(anyhow::anyhow!(
                "chunks/embedding count mismatch: {} chunks vs {} embeddings",
                chunks.len(),
                embeddings.len()
            ));
        }

        let points: Vec<_> = chunks
            .iter()
            .zip(embeddings.iter())
            .map(|(c, emb)| {
                json!({
                    "id": c.id,
                    "vector": emb,
                    "payload": {
                        "file_path": c.file_path,
                        "symbol": c.symbol,
                        "language": c.language,
                        "type": c.kind,
                        "content": c.content,
                        "line_start": c.line_start.to_string(),
                        "line_end": c.line_end.to_string(),
                    }
                })
            })
            .collect();

        let url = format!(
            "{}/collections/{}/points?wait=true",
            self.base_url, self.collection
        );
        let resp = self
            .client
            .put(&url)
            .json(&json!({ "points": points }))
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to upsert to Qdrant: {}. Is Qdrant reachable?", e)
            })?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "Qdrant upsert failed with HTTP {}. Response: {:?}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }
        Ok(())
    }

    async fn search(
        &self,
        embedding: &[f32],
        limit: usize,
        filters: &HashMap<String, String>,
    ) -> Result<Vec<SearchResult>> {
        let mut must = vec![];
        for (k, v) in filters {
            must.push(json!({
                "key": k,
                "match": { "value": v }
            }));
        }

        let mut payload = json!({
            "vector": embedding,
            "limit": limit,
            "with_payload": true,
        });
        if !must.is_empty() {
            payload["filter"] = json!({ "must": must });
        }

        let url = format!(
            "{}/collections/{}/points/search",
            self.base_url, self.collection
        );
        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("failed to search Qdrant: {}. Is Qdrant reachable?", e))?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "Qdrant search failed with HTTP {}. Response: {:?}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let results = body["result"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| {
                let score = r["score"].as_f64()? as f32;
                let p = r["payload"].as_object()?;
                let chunk = Chunk {
                    id: r["id"].as_str().unwrap_or("").into(),
                    file_path: p.get("file_path")?.as_str()?.into(),
                    symbol: p.get("symbol")?.as_str()?.into(),
                    language: p.get("language")?.as_str()?.into(),
                    kind: p.get("type")?.as_str()?.into(),
                    content: p.get("content")?.as_str()?.into(),
                    line_start: p.get("line_start")?.as_str().and_then(|s| s.parse().ok())?,
                    line_end: p.get("line_end")?.as_str().and_then(|s| s.parse().ok())?,
                    meta: Default::default(),
                };
                Some(SearchResult { chunk, score })
            })
            .collect();

        Ok(results)
    }

    async fn delete_by_file(&self, file_path: &str) -> Result<()> {
        let payload = json!({
            "filter": {
                "must": [
                    { "key": "file_path", "match": { "value": file_path } }
                ]
            }
        });
        let url = format!(
            "{}/collections/{}/points/delete?wait=true",
            self.base_url, self.collection
        );
        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to delete from Qdrant: {}. Is Qdrant reachable?", e)
            })?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "Qdrant delete failed with HTTP {}. Response: {:?}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }
        Ok(())
    }
}
