use crate::models::{Chunk, SearchResult};
use anyhow::Result;
use std::collections::HashMap;

#[async_trait::async_trait]
pub trait Store: Send + Sync {
    async fn init(&self, vector_size: usize) -> Result<()>;
    async fn upsert(&self, chunks: &[Chunk], embeddings: &[Vec<f32>]) -> Result<()>;
    async fn search(
        &self,
        embedding: &[f32],
        limit: usize,
        filters: &HashMap<String, String>,
    ) -> Result<Vec<SearchResult>>;
    async fn delete_by_file(&self, file_path: &str) -> Result<()>;
}

pub mod edge;
