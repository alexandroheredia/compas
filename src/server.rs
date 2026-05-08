use crate::config::AppConfig;
use crate::embedder::{EmbedMode, Embedder};
use crate::graph::Graph;
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
                let query_lower = query.to_lowercase();
                let query_tokens: Vec<&str> = query_lower.split_whitespace().collect();
                let query_mentions_private = query_tokens.iter().any(|t| {
                    *t == "private" || *t == "helper" || *t == "internal" || *t == "implementation"
                });
                let boosted: Vec<crate::models::SearchResult> = raw_results
                    .into_iter()
                    .map(|mut r| {
                        let symbol_lower = r.chunk.symbol.to_lowercase();
                        let file_lower = r.chunk.file_path.to_lowercase();
                        let mut boost = 0.0f32;
                        for token in &query_tokens {
                            if symbol_lower.contains(token) {
                                boost += 0.12;
                            }
                            if file_lower.contains(token) {
                                boost += 0.10;
                            }
                        }
                        match r.chunk.kind.as_str() {
                            "class" => boost += 0.05,
                            "method" => boost += 0.02,
                            _ => {}
                        }
                        if let Some(node) = repo.graph.get(&r.chunk.symbol, &r.chunk.file_path) {
                            let related: Vec<String> = node
                                .calls
                                .iter()
                                .chain(node.called_by.iter())
                                .map(|s| s.to_lowercase())
                                .collect();
                            for token in &query_tokens {
                                if related.iter().any(|s| s.contains(token)) {
                                    boost += 0.10;
                                    break;
                                }
                            }
                        }
                        if r.chunk.symbol.starts_with('_') && !query_mentions_private {
                            boost -= 0.15;
                        }
                        r.score += boost;
                        r
                    })
                    .collect();

                fn strip_part_suffix(name: &str) -> &str {
                    name.rfind("_p")
                        .and_then(|i| name[i + 2..].parse::<u32>().ok().map(|_| &name[..i]))
                        .unwrap_or(name)
                }
                let mut best_by_symbol: std::collections::HashMap<
                    (String, String),
                    crate::models::SearchResult,
                > = std::collections::HashMap::new();
                for r in boosted {
                    let stripped = strip_part_suffix(&r.chunk.symbol).to_string();
                    let key = (r.chunk.file_path.clone(), stripped);
                    let should_insert = match best_by_symbol.get(&key) {
                        Some(existing) => r.score > existing.score,
                        None => true,
                    };
                    if should_insert {
                        best_by_symbol.insert(key, r);
                    }
                }

                let mut file_counts: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                let mut results: Vec<crate::models::SearchResult> =
                    best_by_symbol.into_values().collect();
                results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
                results.retain(|r| {
                    let count = file_counts.entry(r.chunk.file_path.clone()).or_insert(0);
                    if *count < 3 {
                        *count += 1;
                        true
                    } else {
                        false
                    }
                });
                results.truncate(limit);

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
