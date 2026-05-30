//! JSON-RPC protocol types for the Codex app-server.
//!
//! Transport: line-delimited JSON over stdin/stdout (NDJSON).
//! Protocol: JSON-RPC 2.0 style — requests have `id`, notifications don't.

use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct JsonRpcRequest {
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Serialize, Debug)]
pub struct JsonRpcNotification {
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A response we send back (e.g. approval decisions).
#[derive(Serialize, Debug)]
pub struct JsonRpcResponseOut {
    pub id: Value,
    pub result: Value,
}

// ---------------------------------------------------------------------------
// Incoming message parsing
// ---------------------------------------------------------------------------

/// Parsed incoming message from Codex stdout.
#[derive(Debug)]
pub enum IncomingMessage {
    /// A response to one of our requests (has `id` + `result` or `error`).
    Response {
        id: u64,
        result: Option<Value>,
        error: Option<Value>,
    },
    /// A notification (has `method`, no `id` — or `id` for approval requests).
    Notification {
        id: Option<Value>,
        method: String,
        params: Value,
    },
}

pub fn parse_incoming(line: &str) -> Result<IncomingMessage, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("JSON parse error: {e}"))?;

    let has_method = v.get("method").and_then(|m| m.as_str()).is_some();
    let has_result = v.get("result").is_some();
    let has_error = v.get("error").is_some();

    if has_method {
        // Notification or approval request (approval requests have both `id` and `method`)
        let method = v["method"].as_str().unwrap().to_string();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let id = v.get("id").cloned();
        Ok(IncomingMessage::Notification { id, method, params })
    } else if has_result || has_error {
        // Response to one of our requests
        let id = v["id"].as_u64().unwrap_or(0);
        let result = v.get("result").cloned();
        let error = v.get("error").cloned();
        Ok(IncomingMessage::Response { id, result, error })
    } else {
        Err(format!("Unknown message shape: {line}"))
    }
}

// ---------------------------------------------------------------------------
// Request constructors
// ---------------------------------------------------------------------------

pub fn initialize_request(client_name: &str, client_version: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        id: next_id(),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "clientInfo": {
                "name": client_name,
                "title": "OpenSwarm",
                "version": client_version,
            },
            "capabilities": {
                "experimentalApi": true
            },
        })),
    }
}

pub fn initialized_notification() -> JsonRpcNotification {
    JsonRpcNotification {
        method: "initialized".to_string(),
        params: None,
    }
}

pub fn thread_start_request(cwd: &str, model: Option<&str>) -> JsonRpcRequest {
    let mut params = serde_json::Map::new();
    params.insert("cwd".to_string(), serde_json::json!(cwd));
    params.insert(
        "approvalPolicy".to_string(),
        serde_json::json!("on-request"),
    );
    params.insert("sandbox".to_string(), serde_json::json!("workspace-write"));
    if let Some(m) = model {
        params.insert("model".to_string(), serde_json::json!(m));
    }

    JsonRpcRequest {
        id: next_id(),
        method: "thread/start".to_string(),
        params: Some(Value::Object(params)),
    }
}

pub fn thread_resume_request(thread_id: &str, cwd: &str, model: Option<&str>) -> JsonRpcRequest {
    let mut params = serde_json::Map::new();
    params.insert("threadId".to_string(), serde_json::json!(thread_id));
    params.insert("cwd".to_string(), serde_json::json!(cwd));
    params.insert(
        "approvalPolicy".to_string(),
        serde_json::json!("on-request"),
    );
    params.insert("sandbox".to_string(), serde_json::json!("workspace-write"));
    if let Some(m) = model {
        params.insert("model".to_string(), serde_json::json!(m));
    }

    JsonRpcRequest {
        id: next_id(),
        method: "thread/resume".to_string(),
        params: Some(Value::Object(params)),
    }
}

pub fn turn_start_request(
    thread_id: &str,
    prompt: &str,
    cwd: &str,
) -> JsonRpcRequest {
    JsonRpcRequest {
        id: next_id(),
        method: "turn/start".to_string(),
        params: Some(serde_json::json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt}],
            "cwd": cwd,
            "approvalPolicy": "on-request",
            "sandboxPolicy": {
                "type": "workspaceWrite",
                "writableRoots": [cwd],
                "networkAccess": true,
            },
        })),
    }
}

pub fn turn_interrupt_request(thread_id: &str, turn_id: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        id: next_id(),
        method: "turn/interrupt".to_string(),
        params: Some(serde_json::json!({
            "threadId": thread_id,
            "turnId": turn_id,
        })),
    }
}

pub fn account_rate_limits_read_request() -> JsonRpcRequest {
    JsonRpcRequest {
        id: next_id(),
        method: "account/rateLimits/read".to_string(),
        params: Some(serde_json::json!({})),
    }
}

pub fn approval_response(request_id: Value, accept: bool) -> JsonRpcResponseOut {
    JsonRpcResponseOut {
        id: request_id,
        result: serde_json::json!({
            "decision": if accept { "accept" } else { "decline" },
        }),
    }
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

/// Serialize a request to an NDJSON line (with trailing newline).
pub fn to_ndjson_request(req: &JsonRpcRequest) -> String {
    let mut s = serde_json::to_string(req).unwrap_or_default();
    s.push('\n');
    s
}

pub fn to_ndjson_notification(notif: &JsonRpcNotification) -> String {
    let mut s = serde_json::to_string(notif).unwrap_or_default();
    s.push('\n');
    s
}

pub fn to_ndjson_response(resp: &JsonRpcResponseOut) -> String {
    let mut s = serde_json::to_string(resp).unwrap_or_default();
    s.push('\n');
    s
}

// ---------------------------------------------------------------------------
// Param extraction helpers
// ---------------------------------------------------------------------------

/// Extract thread_id from params (handles both camelCase and snake_case).
pub fn extract_thread_id(params: &Value) -> Option<String> {
    params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract item_id from params.
pub fn extract_item_id(params: &Value) -> Option<String> {
    params
        .get("itemId")
        .or_else(|| params.get("item_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract delta text from params.
pub fn extract_delta(params: &Value) -> Option<String> {
    params
        .get("delta")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract turn_id from params (nested or flat).
pub fn extract_turn_id(params: &Value) -> Option<String> {
    params
        .get("turn")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            params
                .get("turnId")
                .or_else(|| params.get("turn_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}
