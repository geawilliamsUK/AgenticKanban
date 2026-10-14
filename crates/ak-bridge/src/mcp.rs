//! A small MCP server over Streamable HTTP, enough for tools: `initialize`,
//! `ping`, `tools/list`, `tools/call`, and the notifications a client sends.
//! Every response is a plain JSON body; the server never opens a stream.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{tools, Bridge};

const SERVER_NAME: &str = "agentic-kanban-board";
const DEFAULT_PROTOCOL: &str = "2025-03-26";

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

pub async fn handle(
    State(bridge): State<Bridge>,
    Path(token): Path<String>,
    method: Method,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(ctx) = bridge.run(&token) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown or finished run"})),
        )
            .into_response();
    };
    match method {
        Method::POST => {}
        Method::DELETE => return StatusCode::OK.into_response(),
        // No server-initiated stream is offered.
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }

    let parsed: Result<Value, _> = serde_json::from_slice(&body);
    let Ok(payload) = parsed else {
        return (
            StatusCode::BAD_REQUEST,
            Json(rpc_error(None, -32700, "parse error")),
        )
            .into_response();
    };

    match payload {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(r) = dispatch(&ctx, item).await {
                    out.push(r);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(out)).into_response()
            }
        }
        single => match dispatch(&ctx, single).await {
            Some(r) => Json(r).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// Handles one message. Notifications (no id) produce no response.
async fn dispatch(ctx: &crate::RunContext, message: Value) -> Option<Value> {
    let req: RpcRequest = match serde_json::from_value(message) {
        Ok(r) => r,
        Err(e) => return Some(rpc_error(None, -32600, &format!("invalid request: {e}"))),
    };
    if req.method.starts_with("notifications/") {
        return None;
    }
    let id = req.id.clone();
    let result = match req.method.as_str() {
        "initialize" => {
            let requested = req
                .params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_PROTOCOL);
            json!({
                "protocolVersion": requested,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
                "instructions": tools::INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::list() }),
        "tools/call" => {
            let name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let outcome = tools::call(ctx, &name, args).await;
            json!({
                "content": [{ "type": "text", "text": outcome.text }],
                "isError": outcome.is_error,
            })
        }
        other => return Some(rpc_error(id, -32601, &format!("method not found: {other}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn rpc_error(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
