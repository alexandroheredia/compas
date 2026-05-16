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

        let expected_config = self.edge_config(vector_size);
        let has_existing_data = std::fs::read_dir(&self.shard_path)
            .with_context(|| {
                format!(
                    "failed to inspect edge shard directory at {}",
                    self.shard_path.display()
                )
            })?
            .next()
            .transpose()?
            .is_some();

        let mut guard = self
            .shard
            .lock()
            .map_err(|_| anyhow!("edge shard mutex poisoned"))?;

        if guard.is_some() {
            return Ok(());
        }

        let shard = if has_existing_data {
            EdgeShard::load(&self.shard_path, Some(expected_config))
                .map_err(edge_error)
                .with_context(|| {
                    format!(
                        "existing edge shard at {} is incompatible with vector '{}' and dimension {}. Delete {} and reindex if the embedding model or vector name changed",
                        self.shard_path.display(),
                        self.vector_name,
                        vector_size,
                        self.shard_path.display()
                    )
                })?
        } else {
            EdgeShard::new(&self.shard_path, expected_config)
                .map_err(edge_error)
                .with_context(|| {
                    format!(
                        "failed to initialize edge shard at {}",
                        self.shard_path.display()
                    )
                })?
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Chunk;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_shard_path(test_name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("compas-edge-{test_name}-{nanos}"))
    }

    fn sample_chunk(id: &str, file_path: &str, symbol: &str, language: &str) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: format!("{symbol} body"),
            language: language.to_string(),
            file_path: file_path.to_string(),
            symbol: symbol.to_string(),
            line_start: 1,
            line_end: 5,
            kind: "method".to_string(),
            meta: Default::default(),
        }
    }

    #[tokio::test]
    async fn edge_store_upsert_search_delete_and_reload() {
        let shard_path = temp_shard_path("lifecycle");
        let store = EdgeStore::new(&shard_path, "default");
        store.init(4).await.unwrap();

        let chunks = vec![
            sample_chunk(
                "11111111-1111-1111-1111-111111111111",
                "/tmp/lib/auth.dart",
                "AuthService.login",
                "dart",
            ),
            sample_chunk(
                "22222222-2222-2222-2222-222222222222",
                "/tmp/lib/cache.dart",
                "CacheService.save",
                "dart",
            ),
        ];
        let embeddings = vec![vec![1.0, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0]];

        store.upsert(&chunks, &embeddings).await.unwrap();

        let results = store
            .search(&[1.0, 0.0, 0.0, 0.0], 5, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk.id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(results[0].chunk.symbol, "AuthService.login");

        store.delete_by_file("/tmp/lib/auth.dart").await.unwrap();

        let remaining = store
            .search(&[1.0, 0.0, 0.0, 0.0], 5, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].chunk.file_path, "/tmp/lib/cache.dart");

        drop(store);

        let reopened = EdgeStore::new(&shard_path, "default");
        let persisted = reopened
            .search(&[0.0, 1.0, 0.0, 0.0], 5, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].chunk.symbol, "CacheService.save");

        drop(reopened);
        fs::remove_dir_all(shard_path).unwrap();
    }

    #[tokio::test]
    async fn edge_store_search_respects_filters() {
        let shard_path = temp_shard_path("filters");
        let store = EdgeStore::new(&shard_path, "default");
        store.init(4).await.unwrap();

        let chunks = vec![
            sample_chunk(
                "33333333-3333-3333-3333-333333333333",
                "/tmp/lib/auth.dart",
                "AuthService.login",
                "dart",
            ),
            sample_chunk(
                "44444444-4444-4444-4444-444444444444",
                "/tmp/src/auth.rs",
                "AuthService::login",
                "rust",
            ),
        ];
        let embeddings = vec![vec![0.8, 0.2, 0.0, 0.0], vec![0.8, 0.2, 0.0, 0.0]];
        store.upsert(&chunks, &embeddings).await.unwrap();

        let mut filters = HashMap::new();
        filters.insert("language".to_string(), "dart".to_string());

        let results = store
            .search(&[0.8, 0.2, 0.0, 0.0], 5, &filters)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.language, "dart");
        assert_eq!(results[0].chunk.file_path, "/tmp/lib/auth.dart");

        drop(store);
        fs::remove_dir_all(shard_path).unwrap();
    }

    #[tokio::test]
    async fn edge_store_rejects_non_uuid_or_numeric_chunk_ids() {
        let shard_path = temp_shard_path("invalid-id");
        let store = EdgeStore::new(&shard_path, "default");
        store.init(4).await.unwrap();

        let chunk = sample_chunk(
            "not-a-valid-point-id",
            "/tmp/lib/auth.dart",
            "Bad.id",
            "dart",
        );
        let err = store
            .upsert(&[chunk], &[vec![1.0, 0.0, 0.0, 0.0]])
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("is not a valid Qdrant Edge point id"));

        drop(store);
        fs::remove_dir_all(shard_path).unwrap();
    }

    #[tokio::test]
    async fn edge_store_rejects_incompatible_vector_size() {
        let shard_path = temp_shard_path("vector-size-mismatch");
        let store = EdgeStore::new(&shard_path, "default");
        store.init(4).await.unwrap();
        drop(store);

        let reopened = EdgeStore::new(&shard_path, "default");
        let err = reopened.init(8).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("incompatible with vector 'default' and dimension 8"));

        fs::remove_dir_all(shard_path).unwrap();
    }

    #[tokio::test]
    async fn edge_store_rejects_incompatible_vector_name() {
        let shard_path = temp_shard_path("vector-name-mismatch");
        let store = EdgeStore::new(&shard_path, "default");
        store.init(4).await.unwrap();
        drop(store);

        let reopened = EdgeStore::new(&shard_path, "secondary");
        let err = reopened.init(4).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("incompatible with vector 'secondary' and dimension 4"));

        fs::remove_dir_all(shard_path).unwrap();
    }

    #[tokio::test]
    async fn edge_store_handles_large_batch_and_optimize() {
        let shard_path = temp_shard_path("large-batch");
        let store = EdgeStore::new(&shard_path, "default");
        store.init(4).await.unwrap();

        let chunks: Vec<Chunk> = (0..128)
            .map(|index| {
                sample_chunk(
                    &format!("00000000-0000-0000-0000-{:012}", index + 1),
                    &format!("/tmp/lib/file_{index}.dart"),
                    &format!("Service{index}.run"),
                    "dart",
                )
            })
            .collect();
        let embeddings: Vec<Vec<f32>> = (0..128)
            .map(|index| vec![1.0, index as f32 / 128.0, 0.0, 0.0])
            .collect();

        store.upsert(&chunks, &embeddings).await.unwrap();
        let _ = store.optimize().unwrap();

        let results = store
            .search(&[1.0, 0.0, 0.0, 0.0], 10, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(results.len(), 10);

        drop(store);
        fs::remove_dir_all(shard_path).unwrap();
    }

    #[tokio::test]
    async fn edge_store_supports_concurrent_searches() {
        let shard_path = temp_shard_path("concurrent-search");
        let store = Arc::new(EdgeStore::new(&shard_path, "default"));
        store.init(4).await.unwrap();

        let chunks = vec![sample_chunk(
            "55555555-5555-5555-5555-555555555555",
            "/tmp/lib/auth.dart",
            "AuthService.login",
            "dart",
        )];
        let embeddings = vec![vec![1.0, 0.0, 0.0, 0.0]];
        store.upsert(&chunks, &embeddings).await.unwrap();

        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    store
                        .search(&[1.0, 0.0, 0.0, 0.0], 5, &HashMap::new())
                        .await
                        .unwrap()
                })
            })
            .collect();

        for task in tasks {
            let results = task.await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].chunk.symbol, "AuthService.login");
        }

        drop(store);
        fs::remove_dir_all(shard_path).unwrap();
    }
}
