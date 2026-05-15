use super::Store;
use crate::models::{Chunk, SearchResult};
use anyhow::{anyhow, Context, Result};
use qdrant_edge::{
    Condition, Distance, EdgeConfig, EdgeOptimizersConfig, EdgeShard, EdgeVectorParams,
    FieldCondition, Filter, JsonPath, Match, MatchValue, NamedQuery, Payload, PointId,
    PointInsertOperations, PointOperations, PointStruct, PointStructPersisted, QueryEnum,
    QueryRequest, ScoredPoint, ScoringQuery, UpdateOperation, ValueVariants, VectorInternal,
    Vectors, WithPayloadInterface, WithVector,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct EdgeStore {
    shard_path: PathBuf,
    vector_name: String,
    shard: Arc<Mutex<Option<EdgeShard>>>,
}

impl EdgeStore {
    pub fn new(shard_path: impl Into<PathBuf>, vector_name: impl Into<String>) -> Self {
        Self {
            shard_path: shard_path.into(),
            vector_name: vector_name.into(),
            shard: Arc::new(Mutex::new(None)),
        }
    }

    pub fn shard_path(&self) -> &Path {
        &self.shard_path
    }

    pub fn optimize(&self) -> Result<bool> {
        let guard = self.load_or_get()?;
        let shard = guard
            .as_ref()
            .ok_or_else(|| anyhow!("edge shard unavailable after load"))?;
        shard.optimize().map_err(edge_error)
    }

    fn load_or_get(&self) -> Result<std::sync::MutexGuard<'_, Option<EdgeShard>>> {
        let mut guard = self
            .shard
            .lock()
            .map_err(|_| anyhow!("edge shard mutex poisoned"))?;

        if guard.is_none() {
            let shard = EdgeShard::load(&self.shard_path, None).map_err(edge_error)?;
            *guard = Some(shard);
        }

        Ok(guard)
    }

    fn edge_config(&self, vector_size: usize) -> EdgeConfig {
        let mut config = EdgeConfig::default();
        config.vectors.insert(
            self.vector_name.clone(),
            EdgeVectorParams {
                size: vector_size,
                distance: Distance::Cosine,
                on_disk: Some(true),
                multivector_config: None,
                datatype: None,
                quantization_config: None,
                hnsw_config: None,
            },
        );
        config.optimizers = EdgeOptimizersConfig {
            deleted_threshold: Some(0.2),
            vacuum_min_vector_number: Some(100),
            default_segment_number: Some(2),
            ..Default::default()
        };
        config
    }
}

#[async_trait::async_trait]
impl Store for EdgeStore {
    async fn init(&self, vector_size: usize) -> Result<()> {
        std::fs::create_dir_all(&self.shard_path).with_context(|| {
            format!(
                "failed to create edge shard directory at {}",
                self.shard_path.display()
            )
        })?;

        let mut guard = self
            .shard
            .lock()
            .map_err(|_| anyhow!("edge shard mutex poisoned"))?;

        if guard.is_some() {
            return Ok(());
        }

        let shard = match EdgeShard::load(&self.shard_path, None) {
            Ok(shard) => shard,
            Err(_) => EdgeShard::new(&self.shard_path, self.edge_config(vector_size))
                .map_err(edge_error)
                .with_context(|| {
                    format!(
                        "failed to initialize edge shard at {}",
                        self.shard_path.display()
                    )
                })?,
        };

        *guard = Some(shard);
        Ok(())
    }

    async fn upsert(&self, chunks: &[Chunk], embeddings: &[Vec<f32>]) -> Result<()> {
        if chunks.len() != embeddings.len() {
            return Err(anyhow!(
                "chunks/embedding count mismatch: {} chunks vs {} embeddings",
                chunks.len(),
                embeddings.len()
            ));
        }

        let points: Vec<PointStructPersisted> = chunks
            .iter()
            .zip(embeddings)
            .map(|(chunk, embedding)| -> Result<PointStructPersisted> {
                let id = chunk.id.parse::<PointId>().map_err(|_| {
                    anyhow!(
                        "chunk id '{}' is not a valid Qdrant Edge point id",
                        chunk.id
                    )
                })?;
                Ok(PointStruct::new(
                    id,
                    Vectors::new_named([(self.vector_name.as_str(), embedding.clone())]),
                    serde_json::json!({
                        "chunk_id": chunk.id,
                        "file_path": chunk.file_path,
                        "symbol": chunk.symbol,
                        "language": chunk.language,
                        "type": chunk.kind,
                        "content": chunk.content,
                        "line_start": chunk.line_start,
                        "line_end": chunk.line_end,
                    }),
                )
                .into())
            })
            .collect::<Result<_>>()?;

        let guard = self.load_or_get()?;
        let shard = guard
            .as_ref()
            .ok_or_else(|| anyhow!("edge shard unavailable after load"))?;

        shard
            .update(UpdateOperation::PointOperation(
                PointOperations::UpsertPoints(PointInsertOperations::PointsList(points)),
            ))
            .map_err(edge_error)
            .context("failed to upsert points into edge shard")
    }

    async fn search(
        &self,
        embedding: &[f32],
        limit: usize,
        filters: &HashMap<String, String>,
    ) -> Result<Vec<SearchResult>> {
        let filter = build_filter(filters)?;

        let guard = self.load_or_get()?;
        let shard = guard
            .as_ref()
            .ok_or_else(|| anyhow!("edge shard unavailable after load"))?;

        let results = shard
            .query(QueryRequest {
                prefetches: vec![],
                query: Some(ScoringQuery::Vector(QueryEnum::Nearest(NamedQuery::new(
                    VectorInternal::Dense(embedding.to_vec()),
                    self.vector_name.clone(),
                )))),
                filter,
                score_threshold: None,
                limit,
                offset: 0,
                params: None,
                with_vector: WithVector::Bool(false),
                with_payload: WithPayloadInterface::Bool(true),
            })
            .map_err(edge_error)
            .context("failed to search edge shard")?;

        results
            .into_iter()
            .map(search_result_from_scored_point)
            .collect()
    }

    async fn delete_by_file(&self, file_path: &str) -> Result<()> {
        let guard = self.load_or_get()?;
        let shard = guard
            .as_ref()
            .ok_or_else(|| anyhow!("edge shard unavailable after load"))?;

        let filter = Filter::new_must(Condition::Field(FieldCondition::new_match(
            parse_json_path("file_path")?,
            Match::Value(MatchValue {
                value: ValueVariants::String(file_path.to_string()),
            }),
        )));

        shard
            .update(UpdateOperation::PointOperation(
                PointOperations::DeletePointsByFilter(filter),
            ))
            .map_err(edge_error)
            .with_context(|| format!("failed to delete points for file '{}'", file_path))
    }
}

fn build_filter(filters: &HashMap<String, String>) -> Result<Option<Filter>> {
    if filters.is_empty() {
        return Ok(None);
    }

    let must = filters
        .iter()
        .map(|(key, value)| {
            Ok(Condition::Field(FieldCondition::new_match(
                parse_json_path(key)?,
                Match::Value(MatchValue {
                    value: ValueVariants::String(value.clone()),
                }),
            )))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(Filter {
        should: None,
        min_should: None,
        must: Some(must),
        must_not: None,
    }))
}

fn search_result_from_scored_point(point: ScoredPoint) -> Result<SearchResult> {
    let payload = point
        .payload
        .ok_or_else(|| anyhow!("edge search result missing payload"))?;

    Ok(SearchResult {
        chunk: Chunk {
            id: payload_string(&payload, "chunk_id")?.unwrap_or_else(|| point.id.to_string()),
            file_path: payload_string(&payload, "file_path")?
                .ok_or_else(|| anyhow!("edge search result missing file_path payload"))?,
            symbol: payload_string(&payload, "symbol")?
                .ok_or_else(|| anyhow!("edge search result missing symbol payload"))?,
            language: payload_string(&payload, "language")?
                .ok_or_else(|| anyhow!("edge search result missing language payload"))?,
            kind: payload_string(&payload, "type")?
                .ok_or_else(|| anyhow!("edge search result missing type payload"))?,
            content: payload_string(&payload, "content")?
                .ok_or_else(|| anyhow!("edge search result missing content payload"))?,
            line_start: payload_usize(&payload, "line_start")?
                .ok_or_else(|| anyhow!("edge search result missing line_start payload"))?,
            line_end: payload_usize(&payload, "line_end")?
                .ok_or_else(|| anyhow!("edge search result missing line_end payload"))?,
            meta: Default::default(),
        },
        score: point.score,
    })
}

fn payload_string(payload: &Payload, key: &str) -> Result<Option<String>> {
    match payload.0.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(other) => Err(anyhow!(
            "edge payload key '{}' expected string, got {}",
            key,
            other
        )),
    }
}

fn payload_usize(payload: &Payload, key: &str) -> Result<Option<usize>> {
    match payload.0.get(key) {
        None => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| anyhow!("edge payload key '{}' is not an unsigned integer", key))
            .map(Some),
        Some(other) => Err(anyhow!(
            "edge payload key '{}' expected number, got {}",
            key,
            other
        )),
    }
}

fn edge_error(err: qdrant_edge::OperationError) -> anyhow::Error {
    anyhow!(err.to_string())
}

fn parse_json_path(path: &str) -> Result<JsonPath> {
    path.parse()
        .map_err(|_| anyhow!("invalid edge payload path '{}'", path))
}
