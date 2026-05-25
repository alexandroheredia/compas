use crate::config::AppConfig;
use crate::embedder::{EmbedMode, Embedder};
use crate::graph::Graph;
use crate::search::rerank_results;
use crate::store::Store;
use axum::{extract::Query, middleware, response::Json, routing::get, Router};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

pub struct RepoState {
    pub config: AppConfig,
    pub store: Arc<dyn Store>,
    pub graph: Arc<Graph>,
    pub embedder: Arc<dyn Embedder>,
}

pub struct AppState {
    pub repos: HashMap<String, RepoState>,
    pub default_repo: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/search", get(search_handler))
        .route("/graph", get(graph_handler))
        .route("/repos", get(list_repos))
        .layer(middleware::from_fn(crate::middleware::request_logger))
        .with_state(state)
}

fn resolve_repo<'a>(
    state: &'a AppState,
    params: &HashMap<String, String>,
) -> Result<&'a RepoState, String> {
    let repo_name = params
        .get("repo")
        .cloned()
        .or_else(|| state.default_repo.clone())
        .ok_or_else(|| {
            let available: Vec<String> = state.repos.keys().cloned().collect();
            format!(
                "missing 'repo' parameter. Available repos: {}",
                available.join(", ")
            )
        })?;

    state
        .repos
        .get(&repo_name)
        .ok_or_else(|| format!("repo '{}' not found", repo_name))
}

async fn health(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let mut repo_statuses = serde_json::Map::new();
    for (name, repo) in &state.repos {
        repo_statuses.insert(
            name.clone(),
            json!({
                "status": "registered",
                "path": repo.config.repo.path,
                "store_provider": repo.config.store.provider,
                "vector_name": repo.config.store.vector_name,
            }),
        );
    }

    Json(json!({
        "status": "ok",
        "repos": repo_statuses,
        "default_repo": state.default_repo,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AppConfig, EmbedderConfig, IndexConfig, RepoConfig, ServerConfig, StoreConfig,
    };
    use crate::embedder::{EmbedMode, Embedder};
    use crate::models::{Chunk, SearchResult};
    use anyhow::{anyhow, Result};

    struct PanicStore;

    #[async_trait::async_trait]
    impl Store for PanicStore {
        async fn init(&self, _vector_size: usize) -> Result<()> {
            Err(anyhow!("store.init should not be called by /health"))
        }

        async fn upsert(&self, _chunks: &[Chunk], _embeddings: &[Vec<f32>]) -> Result<()> {
            unreachable!("upsert is not used by /health tests")
        }

        async fn search(
            &self,
            _embedding: &[f32],
            _limit: usize,
            _filters: &HashMap<String, String>,
        ) -> Result<Vec<SearchResult>> {
            unreachable!("search is not used by /health tests")
        }

        async fn delete_by_file(&self, _file_path: &str) -> Result<()> {
            unreachable!("delete_by_file is not used by /health tests")
        }
    }

    struct FakeEmbedder;

    #[async_trait::async_trait]
    impl Embedder for FakeEmbedder {
        async fn embed(&self, _text: &str, _mode: EmbedMode) -> Result<Vec<f32>> {
            Ok(vec![1.0, 0.0, 0.0, 0.0])
        }

        async fn embed_batch(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
        }

        fn dimensions(&self) -> usize {
            4
        }
    }

    fn sample_config() -> AppConfig {
        AppConfig {
            repo: RepoConfig {
                path: "/tmp/bookswipe".to_string(),
                include: vec!["lib/**/*.dart".to_string()],
                exclude: vec![],
            },
            embedder: EmbedderConfig {
                provider: "fastembed".to_string(),
                model: "nomic-ai/nomic-embed-text-v1.5".to_string(),
                query_prefix: None,
                doc_prefix: None,
            },
            store: StoreConfig {
                provider: "edge".to_string(),
                path: ".compas/edge-shard".to_string(),
                vector_name: "default".to_string(),
            },
            server: ServerConfig {
                port: "3001".to_string(),
                host: "127.0.0.1".to_string(),
            },
            index: IndexConfig {
                chunk_by: "function".to_string(),
                watch: true,
            },
        }
    }

    #[tokio::test]
    async fn health_reports_registered_repos_without_touching_store() {
        let state = Arc::new(AppState {
            repos: HashMap::from([(
                "bookswipe".to_string(),
                RepoState {
                    config: sample_config(),
                    store: Arc::new(PanicStore),
                    graph: Arc::new(Graph::new()),
                    embedder: Arc::new(FakeEmbedder),
                },
            )]),
            default_repo: Some("bookswipe".to_string()),
        });

        let Json(value) = health(axum::extract::State(state)).await;

        assert_eq!(value["status"], "ok");
        assert_eq!(value["default_repo"], "bookswipe");
        assert_eq!(value["repos"]["bookswipe"]["status"], "registered");
        assert_eq!(value["repos"]["bookswipe"]["path"], "/tmp/bookswipe");
        assert_eq!(value["repos"]["bookswipe"]["store_provider"], "edge");
        assert_eq!(value["repos"]["bookswipe"]["vector_name"], "default");
    }
}

async fn list_repos(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let repos: Vec<String> = state.repos.keys().cloned().collect();
    Json(json!({
        "repos": repos,
        "default_repo": state.default_repo,
    }))
}

async fn search_handler(
    Query(params): Query<HashMap<String, String>>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let repo = match resolve_repo(&state, &params) {
        Ok(r) => r,
        Err(e) => return Json(json!({"error": e})),
    };

    let query = params.get("q").cloned().unwrap_or_default();
    if query.is_empty() {
        return Json(json!({"error": "missing query"}));
    }

    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(15);

    let mut filters = HashMap::new();
    if let Some(lang) = params.get("language") {
        filters.insert("language".into(), lang.clone());
    }

    match repo.embedder.embed(&query, EmbedMode::Query).await {
        Ok(embedding) => match repo.store.search(&embedding, limit * 3, &filters).await {
            Ok(raw_results) => {
                let results = rerank_results(repo.graph.as_ref(), raw_results, &query, limit);

                Json(json!({
                    "query": query,
                    "results": results,
                }))
            }
            Err(e) => Json(json!({"error": format!("search failed: {}", e)})),
        },
        Err(e) => Json(json!({"error": format!("embedding failed: {}", e)})),
    }
}

async fn graph_handler(
    Query(params): Query<HashMap<String, String>>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let repo = match resolve_repo(&state, &params) {
        Ok(r) => r,
        Err(e) => return Json(json!({"error": e})),
    };

    let symbol = params.get("symbol").cloned().unwrap_or_default();
    let file = params.get("file").cloned().unwrap_or_default();
    if symbol.is_empty() {
        return Json(json!({"error": "missing symbol"}));
    }
    let exact = repo.graph.get(&symbol, &file);
    let matches = if let Some(node) = exact {
        vec![node]
    } else {
        repo.graph.search(&symbol)
    };
    if matches.is_empty() {
        Json(json!({"error": "symbol not found"}))
    } else {
        Json(json!(matches))
    }
}
