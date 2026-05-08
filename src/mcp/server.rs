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

async fn handle_request(state: &Arc<McpAppState>, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
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
