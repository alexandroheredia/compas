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
    let mut healthy = true;

    // Check all repos
    let mut repo_statuses = serde_json::Map::new();
    for (name, repo) in &state.repos {
        let mut repo_ok = true;
        match repo.store.init(768).await {
            Ok(_) => {}
            Err(e) => {
                repo_statuses.insert(name.clone(), json!({ "store": format!("error: {}", e) }));
                repo_ok = false;
            }
        }
        if repo_ok {
            repo_statuses.insert(name.clone(), json!({ "status": "ok" }));
        } else {
            healthy = false;
        }
    }

    let status = if healthy { "ok" } else { "degraded" };
    Json(json!({
        "status": status,
        "repos": repo_statuses,
        "default_repo": state.default_repo,
    }))
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
