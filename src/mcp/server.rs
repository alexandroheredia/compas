use super::state::McpAppState;
use super::tools;
use super::types::*;
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info};

pub async fn run_stdio_server(state: Arc<McpAppState>) -> anyhow::Result<()> {
    info!("compas MCP server started on stdio");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut stdout = stdout;
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }

        debug!("mcp recv: {}", line);

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError::invalid_request(format!(
                        "invalid JSON: {}",
                        e
                    ))),
                };
                send_response(&mut stdout, &resp).await;
                continue;
            }
        };

        let resp = handle_request(&state, req).await;
        if let Some(r) = resp {
            send_response(&mut stdout, &r).await;
        }
    }

    info!("mcp server shutting down");
    Ok(())
}

async fn send_response(stdout: &mut tokio::io::Stdout, resp: &JsonRpcResponse) {
    let json = match serde_json::to_string(resp) {
        Ok(j) => j,
        Err(e) => {
            error!("failed to serialize response: {}", e);
            return;
        }
    };
    let line = format!("{}\n", json);
    if let Err(e) = stdout.write_all(line.as_bytes()).await {
        error!("failed to write response: {}", e);
        return;
    }
    if let Err(e) = stdout.flush().await {
        error!("failed to flush stdout: {}", e);
    }
    debug!("mcp send: {}", json);
}

pub(crate) async fn handle_request(
    state: &Arc<McpAppState>,
    req: JsonRpcRequest,
) -> Option<JsonRpcResponse> {
    let id = req.id.clone();

    match req.method.as_str() {
        "initialize" => Some(handle_initialize(id, &req.params)),
        "initialized" => {
            // Notification, no response
            None
        }
        "tools/list" => Some(handle_tools_list(id)),
        "tools/call" => handle_tools_call(state, id, &req.params).await,
        _ => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError::method_not_found(format!(
                "method '{}' not found",
                req.method
            ))),
        }),
    }
}

fn handle_initialize(
    id: Option<serde_json::Value>,
    _params: &serde_json::Value,
) -> JsonRpcResponse {
    let result = InitializeResult {
        protocol_version: "2024-11-05".into(),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability {}),
        },
        server_info: ServerInfo {
            name: "compas".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
    };

    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(json!(result)),
        error: None,
    }
}

fn handle_tools_list(id: Option<serde_json::Value>) -> JsonRpcResponse {
    let tools = tools::list_tools();
    let result = ToolsListResult { tools };

    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(json!(result)),
        error: None,
    }
}

async fn handle_tools_call(
    state: &Arc<McpAppState>,
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
) -> Option<JsonRpcResponse> {
    let call_params: ToolCallParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return Some(JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(JsonRpcError::invalid_params(format!(
                    "bad tool call params: {}",
                    e
                ))),
            });
        }
    };

    match tools::handle_tool_call(state, &call_params.name, &call_params.arguments).await {
        Ok(result) => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(json!(result)),
            error: None,
        }),
        Err(e) => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(json!(ToolCallResult {
                content: vec![ToolContent {
                    kind: "text".into(),
                    text: format!("Error: {}", e),
                }],
                is_error: Some(true),
            })),
            error: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::{EmbedMode, Embedder};
    use crate::graph::Graph;
    use crate::mcp::state::RepoState;
    use crate::models::Chunk;
    use crate::store::edge::EdgeStore;
    use crate::store::Store;
    use anyhow::Result;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FakeEmbedder;

    #[async_trait::async_trait]
    impl Embedder for FakeEmbedder {
        async fn embed(&self, text: &str, _mode: EmbedMode) -> Result<Vec<f32>> {
            Ok(test_embedding(text))
        }

        async fn embed_batch(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|text| test_embedding(text)).collect())
        }

        fn dimensions(&self) -> usize {
            4
        }
    }

    fn test_embedding(text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        if lower.contains("auth") || lower.contains("login") {
            vec![1.0, 0.0, 0.0, 0.0]
        } else {
            vec![0.0, 1.0, 0.0, 0.0]
        }
    }

    fn temp_shard_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("compas-mcp-test-{name}-{nanos}"))
    }

    fn sample_chunk() -> Chunk {
        Chunk {
            id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
            content: "auth_service.dart AuthService.login\nFuture<String> login() async {}"
                .to_string(),
            language: "dart".to_string(),
            file_path: "/tmp/lib/auth_service.dart".to_string(),
            symbol: "AuthService.login".to_string(),
            line_start: 1,
            line_end: 3,
            kind: "method".to_string(),
            meta: Default::default(),
        }
    }

    #[tokio::test]
    async fn tools_call_search_codebase_returns_edge_results() {
        let shard_path = temp_shard_path("search-codebase");
        let store_impl = Arc::new(EdgeStore::new(&shard_path, "default"));
        store_impl.init(4).await.unwrap();

        let chunk = sample_chunk();
        store_impl
            .upsert(&[chunk], &[vec![1.0, 0.0, 0.0, 0.0]])
            .await
            .unwrap();

        let graph = Arc::new(Graph::new());
        graph.add_symbol("AuthService.login", "/tmp/lib/auth_service.dart", "method");

        let state = Arc::new(McpAppState {
            repos: HashMap::from([(
                "test-repo".to_string(),
                RepoState {
                    store: store_impl.clone() as Arc<dyn Store>,
                    graph,
                    embedder: Arc::new(FakeEmbedder),
                },
            )]),
            default_repo: Some("test-repo".to_string()),
        });

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "tools/call".to_string(),
            params: json!({
                "name": "search_codebase",
                "arguments": {
                    "query": "authentication",
                    "limit": 5
                }
            }),
        };

        let response = handle_request(&state, request).await.unwrap();
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );

        let result = response.result.unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("AuthService.login"),
            "unexpected MCP text: {text}"
        );
        assert!(
            text.contains("auth_service.dart"),
            "unexpected MCP text: {text}"
        );

        drop(state);
        drop(store_impl);
        std::fs::remove_dir_all(shard_path).unwrap();
    }
}
