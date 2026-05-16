use super::state::{McpAppState, RepoState};
use super::types::*;
use crate::embedder::EmbedMode;
use crate::models::SearchResult;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

pub fn list_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "search_codebase".into(),
            description: "Find code in the repository using natural language semantic search. Returns relevant files, functions, classes, and code snippets with file paths and line numbers. Use this when looking for specific functionality, features, or implementations across the entire codebase.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural language query describing what you are looking for (e.g. 'user authentication', 'image caching', 'database models')" },
                    "limit": { "type": "number", "description": "Maximum number of results to return (default: 5)" },
                    "language": { "type": "string", "description": "Optional language filter, e.g. 'dart'" },
                    "repo": { "type": "string", "description": "Optional repo name (e.g. 'my-app'). Only needed if the daemon serves multiple repos." }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "get_symbol_graph".into(),
            description: "Get the relationships and dependencies for a function, method, or class. Shows what the symbol calls (outgoing dependencies) and what other functions or classes call it (incoming dependencies or callers). Use this to trace code paths, understand impact of changes, or find how a function is used.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Name of the function, method, or class to analyze (e.g. 'AuthService.login', 'CacheService', 'getUserById')" },
                    "file": { "type": "string", "description": "Optional file path to disambiguate symbols with the same name, e.g. 'lib/services/auth_service.dart'" },
                    "repo": { "type": "string", "description": "Optional repo name (e.g. 'my-app'). Only needed if the daemon serves multiple repos." }
                },
                "required": ["symbol"]
            }),
        },
    ]
}

fn resolve_repo<'a>(
    state: &'a McpAppState,
    args: &serde_json::Value,
) -> Result<(&'a str, &'a RepoState), String> {
    let repo_name = args["repo"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| {
            // Try to auto-detect from cwd
            let cwd = std::env::current_dir().ok()?;
            let cwd_lower = cwd.to_string_lossy().to_lowercase();
            for name in state.repos.keys() {
                // Case-insensitive heuristic: match if cwd contains the repo name
                if cwd_lower.contains(&name.to_lowercase()) {
                    return Some(name.clone());
                }
            }
            state.default_repo.clone()
        })
        .ok_or_else(|| {
            let available: Vec<String> = state.repos.keys().cloned().collect();
            format!(
                "missing 'repo' parameter and could not auto-detect from cwd. Available repos: {}",
                available.join(", ")
            )
        })?;

    state
        .repos
        .iter()
        .find(|(name, _)| name.to_lowercase() == repo_name.to_lowercase())
        .map(|(name, repo)| (name.as_str(), repo))
        .ok_or_else(|| format!("repo '{}' not found", repo_name))
}

pub async fn handle_tool_call(
    state: &McpAppState,
    name: &str,
    args: &serde_json::Value,
) -> Result<ToolCallResult, String> {
    match name {
        "search_codebase" => handle_search(state, args).await,
        "get_symbol_graph" => handle_graph(state, args).await,
        _ => Err(format!("unknown tool: {}", name)),
    }
}

async fn handle_search(
    state: &McpAppState,
    args: &serde_json::Value,
) -> Result<ToolCallResult, String> {
    let query = args["query"].as_str().ok_or("missing 'query' argument")?;
    let limit = args["limit"].as_u64().unwrap_or(10) as usize;
    let language = args["language"].as_str();

    let (repo_name, repo) = resolve_repo(state, args)?;

    let embedding = repo
        .embedder
        .embed(query, EmbedMode::Query)
        .await
        .map_err(|e| format!("embed failed: {}", e))?;

    let mut filters = HashMap::new();
    if let Some(lang) = language {
        filters.insert("language".into(), lang.into());
    }

    let results = match repo.store.search(&embedding, limit * 3, &filters).await {
        Ok(raw_results) => rerank_results(repo, raw_results, query, limit),
        Err(e) if is_edge_lock_error(&e.to_string()) => {
            search_via_daemon(query, limit, language, repo_name).await?
        }
        Err(e) => return Err(format!("search failed: {}", e)),
    };

    let text = format_search_results(&results);

    Ok(ToolCallResult {
        content: vec![ToolContent {
            kind: "text".into(),
            text,
        }],
        is_error: None,
    })
}

fn rerank_results(
    repo: &RepoState,
    raw_results: Vec<SearchResult>,
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    // Boost scores based on keyword matches in symbol name, file path, kind,
    // graph relationships, and private-helper penalty.
    let query_lower = query.to_lowercase();
    let query_tokens: Vec<&str> = query_lower.split_whitespace().collect();
    let query_mentions_private = query_tokens
        .iter()
        .any(|t| *t == "private" || *t == "helper" || *t == "internal" || *t == "implementation");
    let boosted: Vec<SearchResult> = raw_results
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
            // Graph cross-reference boost: if callers or callees contain query tokens,
            // the symbol is likely part of the relevant subsystem even if its own text
            // doesn't mention the query terms.
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
                        break; // one boost per result regardless of how many relations match
                    }
                }
            }
            // Penalise private helpers unless the user is explicitly looking for them.
            if r.chunk.symbol.starts_with('_') && !query_mentions_private {
                boost -= 0.15;
            }
            r.score += boost;
            r
        })
        .collect();

    // Deduplicate by (file_path, stripped_symbol) so different symbols from the
    // same file are preserved, but part-chunks (_p1, _p2) of the same symbol are collapsed.
    // Cap at 3 symbols per file to preserve diversity across the codebase.
    fn strip_part_suffix(name: &str) -> &str {
        name.rfind("_p")
            .and_then(|i| name[i + 2..].parse::<u32>().ok().map(|_| &name[..i]))
            .unwrap_or(name)
    }
    let mut best_by_symbol: std::collections::HashMap<(String, String), SearchResult> =
        std::collections::HashMap::new();
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
    let mut results: Vec<SearchResult> = best_by_symbol.into_values().collect();
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

    results
}

fn format_search_results(results: &[SearchResult]) -> String {
    let repo_path = std::env::current_dir().unwrap_or_default();

    if results.is_empty() {
        "No relevant code found.".into()
    } else {
        let mut lines = vec![format!("Found {} relevant file(s):", results.len())];
        for (i, r) in results.iter().enumerate() {
            // Convert absolute path to relative
            let rel_path = std::path::Path::new(&r.chunk.file_path)
                .strip_prefix(&repo_path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| r.chunk.file_path.clone());

            lines.push(format!(
                "\n{}. {}:{}-{}
   Symbol: {}
   Score: {:.3}",
                i + 1,
                rel_path,
                r.chunk.line_start,
                r.chunk.line_end,
                r.chunk.symbol,
                r.score
            ));
            // Include a preview of the content (first 800 chars)
            let preview: String = r.chunk.content.chars().take(800).collect();
            lines.push(format!("   Preview:\n```dart\n{}...\n```", preview));
        }
        lines.join("\n")
    }
}

async fn search_via_daemon(
    query: &str,
    limit: usize,
    language: Option<&str>,
    repo_name: &str,
) -> Result<Vec<SearchResult>, String> {
    #[derive(Deserialize)]
    struct SearchResponse {
        results: Vec<SearchResult>,
    }

    let host = std::env::var("COMPAS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("COMPAS_PORT").unwrap_or_else(|_| "3001".into());
    let url = format!("http://{}:{}/search", host, port);

    let mut params: Vec<(&str, String)> = vec![
        ("q", query.to_string()),
        ("limit", limit.to_string()),
        ("repo", repo_name.to_string()),
    ];
    if let Some(language) = language {
        params.push(("language", language.to_string()));
    }

    let response = reqwest::Client::new()
        .get(&url)
        .query(&params)
        .send()
        .await
        .map_err(|e| format!("daemon search request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!(
            "daemon search failed with HTTP {}",
            response.status()
        ));
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("failed to decode daemon search response: {}", e))?;

    if let Some(error) = payload.get("error").and_then(|value| value.as_str()) {
        return Err(format!("daemon search failed: {}", error));
    }

    serde_json::from_value::<SearchResponse>(payload)
        .map(|response| response.results)
        .map_err(|e| format!("failed to parse daemon search results: {}", e))
}

fn is_edge_lock_error(error: &str) -> bool {
    error.contains("failed to open WAL") || error.contains("Resource temporarily unavailable")
}

async fn handle_graph(
    state: &McpAppState,
    args: &serde_json::Value,
) -> Result<ToolCallResult, String> {
    let symbol = args["symbol"].as_str().ok_or("missing 'symbol' argument")?;
    let file = args["file"].as_str().unwrap_or("");

    let (_, repo) = resolve_repo(state, args)?;

    // Try exact lookup first
    let exact = repo.graph.get(symbol, file);

    // If no exact match, try fuzzy search
    let matches = if let Some(node) = exact {
        vec![node]
    } else {
        repo.graph.search(symbol)
    };

    let text = if matches.is_empty() {
        format!("Symbol '{}' not found in graph.", symbol)
    } else {
        let mut lines = vec![format!(
            "Found {} symbol(s) matching '{}':",
            matches.len(),
            symbol
        )];
        for (i, n) in matches.iter().enumerate() {
            lines.push(format!("\n{}. {} ({}) — {}", i + 1, n.name, n.kind, n.file));
            if !n.calls.is_empty() {
                lines.push(format!("   Calls: {}", n.calls.join(", ")));
            }
            if !n.called_by.is_empty() {
                lines.push(format!("   Called by: {}", n.called_by.join(", ")));
            }
        }
        lines.join("\n")
    };

    Ok(ToolCallResult {
        content: vec![ToolContent {
            kind: "text".into(),
            text,
        }],
        is_error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::{EmbedMode, Embedder};
    use crate::graph::Graph;
    use crate::mcp::state::RepoState;
    use crate::models::Chunk;
    use crate::store::Store;
    use anyhow::{anyhow, Result};
    use axum::{routing::get, Json, Router};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
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

    struct LockedStore;

    #[async_trait::async_trait]
    impl Store for LockedStore {
        async fn init(&self, _vector_size: usize) -> Result<()> {
            Ok(())
        }

        async fn upsert(&self, _chunks: &[Chunk], _embeddings: &[Vec<f32>]) -> Result<()> {
            Err(anyhow!("not implemented"))
        }

        async fn search(
            &self,
            _embedding: &[f32],
            _limit: usize,
            _filters: &HashMap<String, String>,
        ) -> Result<Vec<SearchResult>> {
            Err(anyhow!(
                "Service runtime error: failed to open WAL /tmp/test/wal: Resource temporarily unavailable"
            ))
        }

        async fn delete_by_file(&self, _file_path: &str) -> Result<()> {
            Err(anyhow!("not implemented"))
        }
    }

    #[tokio::test]
    async fn search_codebase_falls_back_to_daemon_on_edge_lock() {
        let _guard = env_lock().lock().unwrap();

        async fn search_handler() -> Json<Value> {
            Json(json!({
                "query": "authentication",
                "results": [{
                    "chunk": {
                        "id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                        "content": "auth_service.dart AuthService.login\nFuture<void> login() async {}",
                        "language": "dart",
                        "file_path": "/tmp/lib/auth_service.dart",
                        "symbol": "AuthService.login",
                        "line_start": 1,
                        "line_end": 2,
                        "type": "method",
                        "meta": {}
                    },
                    "score": 0.99
                }]
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let server = tokio::spawn(async move {
            let app = Router::new().route("/search", get(search_handler));
            axum::serve(listener, app).await.unwrap();
        });

        let original_host = std::env::var_os("COMPAS_HOST");
        let original_port = std::env::var_os("COMPAS_PORT");
        std::env::set_var("COMPAS_HOST", "127.0.0.1");
        std::env::set_var("COMPAS_PORT", &port);

        let state = McpAppState {
            repos: HashMap::from([(
                "bookswipe".to_string(),
                RepoState {
                    store: Arc::new(LockedStore),
                    graph: Arc::new(Graph::new()),
                    embedder: Arc::new(FakeEmbedder),
                },
            )]),
            default_repo: Some("bookswipe".to_string()),
        };

        let result = handle_tool_call(
            &state,
            "search_codebase",
            &json!({"query": "authentication", "repo": "bookswipe", "limit": 5}),
        )
        .await
        .unwrap();

        let text = &result.content[0].text;
        assert!(
            text.contains("AuthService.login"),
            "unexpected fallback text: {text}"
        );

        server.abort();
        let _ = server.await;

        match original_host {
            Some(value) => std::env::set_var("COMPAS_HOST", value),
            None => std::env::remove_var("COMPAS_HOST"),
        }
        match original_port {
            Some(value) => std::env::set_var("COMPAS_PORT", value),
            None => std::env::remove_var("COMPAS_PORT"),
        }
    }
}
