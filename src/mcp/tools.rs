use super::state::{McpAppState, RepoState};
use super::types::*;
use crate::embedder::EmbedMode;
use crate::models::SearchResult;
use crate::search::rerank_results;
use serde_json::json;
use std::collections::HashMap;

pub fn list_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "search_codebase".into(),
            description: "MANDATORY FIRST STEP for any codebase exploration task. Use this BEFORE reading files, listing directories, or searching with regex. Finds code by natural language semantic search and returns the exact files, functions, classes, and line numbers you need. Only read files AFTER this tool confirms their relevance.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural language query describing what you are looking for (e.g. 'user authentication', 'image caching', 'database models')" },
                    "limit": { "type": "number", "description": "Maximum number of results to return (default: 10)" },
                    "language": { "type": "string", "description": "Optional language filter, e.g. 'dart'" },
                    "repo": { "type": "string", "description": "Repository name (e.g. 'my-app'). Always pass this to avoid cwd auto-detection failures." }
                },
                "required": ["query", "repo"]
            }),
        },
        ToolDefinition {
            name: "get_symbol_graph".into(),
            description: "Trace callers and callees for a specific symbol. Use AFTER search_codebase when you need to understand how a function or class is used, or what it depends on. Do not use this for discovery — search first, then graph.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Name of the function, method, or class to analyze (e.g. 'AuthService.login', 'CacheService', 'getUserById')" },
                    "file": { "type": "string", "description": "Optional file path to disambiguate symbols with the same name, e.g. 'lib/services/auth_service.dart'" },
                    "repo": { "type": "string", "description": "Repository name (e.g. 'my-app'). Always pass this to avoid cwd auto-detection failures." }
                },
                "required": ["symbol", "repo"]
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

    let (_, repo) = resolve_repo(state, args)?;

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
        Ok(raw_results) => rerank_results(repo.graph.as_ref(), raw_results, query, limit),
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
    use anyhow::Result;
    use serde_json::json;
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

    #[tokio::test]
    async fn search_codebase_reads_from_edge_store_without_daemon_fallback() {
        let _guard = env_lock().lock().unwrap();

        let shard_path =
            std::env::temp_dir().join(format!("compas-mcp-tools-{}", uuid::Uuid::new_v4()));
        let store = Arc::new(crate::store::edge::EdgeStore::new(&shard_path, "default"));
        store.init(4).await.unwrap();
        store
            .upsert(
                &[Chunk {
                    id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
                    content: "auth_service.dart AuthService.login\nFuture<void> login() async {}"
                        .to_string(),
                    language: "dart".to_string(),
                    file_path: "/tmp/lib/auth_service.dart".to_string(),
                    symbol: "AuthService.login".to_string(),
                    line_start: 1,
                    line_end: 2,
                    kind: "method".to_string(),
                    meta: Default::default(),
                }],
                &[vec![1.0, 0.0, 0.0, 0.0]],
            )
            .await
            .unwrap();

        let state = McpAppState {
            repos: HashMap::from([(
                "bookswipe".to_string(),
                RepoState {
                    store,
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
            "unexpected search text: {text}"
        );

        std::fs::remove_dir_all(shard_path).unwrap();
    }
}
