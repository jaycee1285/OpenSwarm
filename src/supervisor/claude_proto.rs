//! Claude Code WebSocket NDJSON wire protocol types.
//!
//! These types model the messages exchanged between OpenSwarm (acting as a
//! WebSocket server) and the Claude Code CLI (acting as a WS client via
//! `--sdk-url`).  The wire format is newline-delimited JSON (NDJSON).
//!
//! Reference: WEBSOCKET_PROTOCOL_REVERSED.md

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Content blocks (shared between assistant messages and user messages)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },

    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: Value, // string or array of ContentBlock
        #[serde(default)]
        is_error: bool,
    },

    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_tokens: Option<u64>,
    },
}

// ---------------------------------------------------------------------------
// Usage info
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub web_search_requests: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub max_output_tokens: u64,
}

// ---------------------------------------------------------------------------
// Messages from CLI → Server (incoming)
// ---------------------------------------------------------------------------

/// `system/init` — first message after WS connect.
#[derive(Debug, Clone, Deserialize)]
pub struct SystemInit {
    pub session_id: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub claude_code_version: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
}

/// `assistant` — full LLM response.
#[derive(Debug, Clone, Deserialize)]
pub struct AssistantMessage {
    pub message: AssistantBody,
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssistantBody {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// `stream_event` — token-by-token streaming (requires `--verbose`).
#[derive(Debug, Clone, Deserialize)]
pub struct StreamEvent {
    pub event: Value, // Anthropic BetaRawMessageStreamEvent
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `result` — query complete.
#[derive(Debug, Clone, Deserialize)]
pub struct ResultMessage {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub errors: Option<Vec<String>>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub duration_api_ms: u64,
    #[serde(default)]
    pub num_turns: u64,
    #[serde(default)]
    pub total_cost_usd: f64,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default, rename = "modelUsage")]
    pub model_usage: Option<std::collections::HashMap<String, ModelUsage>>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `tool_progress` — heartbeat during tool execution.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolProgress {
    pub tool_use_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub elapsed_time_seconds: f64,
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `control_request` from CLI — permission check.
#[derive(Debug, Clone, Deserialize)]
pub struct ControlRequest {
    pub request_id: String,
    pub request: Value, // parse subtype from this
}

/// Parsed `can_use_tool` payload.
#[derive(Debug, Clone, Deserialize)]
pub struct CanUseToolRequest {
    pub tool_name: String,
    pub input: Value,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

/// `system/status` — compaction status change.
#[derive(Debug, Clone, Deserialize)]
pub struct SystemStatus {
    pub status: Option<String>, // "compacting" or null
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Messages from Server → CLI (outgoing)
// ---------------------------------------------------------------------------

/// `user` message — send a prompt.
#[derive(Debug, Clone, Serialize)]
pub struct UserMessage {
    #[serde(rename = "type")]
    pub msg_type: &'static str, // always "user"
    pub message: UserBody,
    pub parent_tool_use_id: Option<String>,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserBody {
    pub role: &'static str, // always "user"
    pub content: String,
}

impl UserMessage {
    pub fn new(content: String, session_id: String) -> Self {
        Self {
            msg_type: "user",
            message: UserBody {
                role: "user",
                content,
            },
            parent_tool_use_id: None,
            session_id,
        }
    }
}

/// `control_response` — respond to a permission request.
#[derive(Debug, Clone, Serialize)]
pub struct ControlResponse {
    #[serde(rename = "type")]
    pub msg_type: &'static str, // always "control_response"
    pub response: ControlResponseBody,
}

#[derive(Debug, Clone, Serialize)]
pub struct ControlResponseBody {
    pub subtype: &'static str, // "success" or "error"
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    /// Allow a tool use — `updatedInput` is required.
    pub fn allow(request_id: String, updated_input: Value) -> Self {
        let response = serde_json::json!({
            "behavior": "allow",
            "updatedInput": updated_input,
        });
        Self {
            msg_type: "control_response",
            response: ControlResponseBody {
                subtype: "success",
                request_id,
                response: Some(response),
                error: None,
            },
        }
    }

    /// Deny a tool use.
    pub fn deny(request_id: String, message: String) -> Self {
        let response = serde_json::json!({
            "behavior": "deny",
            "message": message,
        });
        Self {
            msg_type: "control_response",
            response: ControlResponseBody {
                subtype: "success",
                request_id,
                response: Some(response),
                error: None,
            },
        }
    }
}

/// `control_request` from server (e.g. `interrupt`, `initialize`).
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingControlRequest {
    #[serde(rename = "type")]
    pub msg_type: &'static str, // always "control_request"
    pub request_id: String,
    pub request: Value,
}

impl OutgoingControlRequest {
    pub fn interrupt(request_id: String) -> Self {
        Self {
            msg_type: "control_request",
            request_id,
            request: serde_json::json!({ "subtype": "interrupt" }),
        }
    }
}

/// `keep_alive` message.
#[derive(Debug, Clone, Serialize)]
pub struct KeepAlive {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
}

impl Default for KeepAlive {
    fn default() -> Self {
        Self {
            msg_type: "keep_alive",
        }
    }
}

// ---------------------------------------------------------------------------
// Parsed envelope — the driver matches on this after initial JSON parse
// ---------------------------------------------------------------------------

/// Top-level discriminant for incoming NDJSON messages.
#[derive(Debug)]
pub enum IncomingMessage {
    SystemInit(SystemInit),
    SystemStatus(SystemStatus),
    Assistant(AssistantMessage),
    StreamEvent(StreamEvent),
    Result(ResultMessage),
    ToolProgress(ToolProgress),
    ControlRequest(ControlRequest),
    KeepAlive,
    /// Anything we don't need to handle — logged and skipped.
    Unknown { msg_type: String },
}

/// Parse a single NDJSON line into an `IncomingMessage`.
///
/// Strategy: parse as `Value` first, then match on `type` (and `subtype` for
/// `system` messages) to deserialize into the appropriate struct.  This is
/// more robust than serde's internally-tagged enum because the protocol has
/// heterogeneous shapes under the same `type` discriminant.
pub fn parse_incoming(line: &str) -> Result<IncomingMessage, serde_json::Error> {
    let v: Value = serde_json::from_str(line)?;

    let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match msg_type {
        "system" => {
            let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            match subtype {
                "init" => {
                    let init: SystemInit = serde_json::from_value(v)?;
                    Ok(IncomingMessage::SystemInit(init))
                }
                "status" => {
                    let status: SystemStatus = serde_json::from_value(v)?;
                    Ok(IncomingMessage::SystemStatus(status))
                }
                // SessionStart hooks send hook_response before system/init — ignore silently
                "hook_response" => Ok(IncomingMessage::KeepAlive),
                _ => Ok(IncomingMessage::Unknown {
                    msg_type: format!("system/{}", subtype),
                }),
            }
        }
        "assistant" => {
            let msg: AssistantMessage = serde_json::from_value(v)?;
            Ok(IncomingMessage::Assistant(msg))
        }
        "stream_event" => {
            let msg: StreamEvent = serde_json::from_value(v)?;
            Ok(IncomingMessage::StreamEvent(msg))
        }
        "result" => {
            let msg: ResultMessage = serde_json::from_value(v)?;
            Ok(IncomingMessage::Result(msg))
        }
        "tool_progress" => {
            let msg: ToolProgress = serde_json::from_value(v)?;
            Ok(IncomingMessage::ToolProgress(msg))
        }
        "control_request" => {
            let msg: ControlRequest = serde_json::from_value(v)?;
            Ok(IncomingMessage::ControlRequest(msg))
        }
        "keep_alive" => Ok(IncomingMessage::KeepAlive),
        // Tool results come back as "user" type messages from the CLI —
        // these are internal bookkeeping and don't need rendering.
        "user" => Ok(IncomingMessage::KeepAlive),
        other => Ok(IncomingMessage::Unknown {
            msg_type: other.to_string(),
        }),
    }
}

/// Serialize an outgoing message as an NDJSON line (with trailing newline).
pub fn to_ndjson<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    Ok(s)
}

// ---------------------------------------------------------------------------
// Helpers for extracting stream_event deltas
// ---------------------------------------------------------------------------

/// Extract text delta from a `stream_event` message.
///
/// The event field contains an Anthropic streaming event.  For
/// `content_block_delta` events with `text_delta` type, we extract the text.
pub fn extract_stream_text(event: &StreamEvent) -> Option<&str> {
    let ev = &event.event;
    let ev_type = ev.get("type")?.as_str()?;
    if ev_type == "content_block_delta" {
        let delta = ev.get("delta")?;
        let delta_type = delta.get("type")?.as_str()?;
        if delta_type == "text_delta" {
            return delta.get("text")?.as_str();
        }
    }
    None
}

/// Extract thinking delta from a `stream_event` message.
pub fn extract_stream_thinking(event: &StreamEvent) -> Option<&str> {
    let ev = &event.event;
    let ev_type = ev.get("type")?.as_str()?;
    if ev_type == "content_block_delta" {
        let delta = ev.get("delta")?;
        let delta_type = delta.get("type")?.as_str()?;
        if delta_type == "thinking_delta" {
            return delta.get("thinking")?.as_str();
        }
    }
    None
}

/// Check if this stream_event signals a content block start.
/// Returns the block type (e.g. "text", "tool_use", "thinking") if so.
pub fn stream_event_block_type(event: &StreamEvent) -> Option<&str> {
    let ev = &event.event;
    let ev_type = ev.get("type")?.as_str()?;
    if ev_type == "content_block_start" {
        let block = ev.get("content_block")?;
        return block.get("type")?.as_str();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_system_init() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/home/user/project","session_id":"abc123","tools":["Bash","Read","Write"],"model":"claude-sonnet-4-5-20250929","permissionMode":"default","apiKeySource":"env","claude_code_version":"2.1.37","slash_commands":[],"output_style":"normal","uuid":"uuid-1","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::SystemInit(init) => {
                assert_eq!(init.session_id, "abc123");
                assert_eq!(init.model, "claude-sonnet-4-5-20250929");
                assert_eq!(init.tools.len(), 3);
                assert_eq!(init.tools[0], "Bash");
            }
            other => panic!("Expected SystemInit, got {:?}", other),
        }
    }

    #[test]
    fn parse_assistant() {
        let line = r#"{"type":"assistant","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[{"type":"text","text":"Hello!"}],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"parent_tool_use_id":null,"uuid":"uuid-2","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::Assistant(a) => {
                assert_eq!(a.message.content.len(), 1);
                match &a.message.content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "Hello!"),
                    other => panic!("Expected Text block, got {:?}", other),
                }
                let usage = a.message.usage.unwrap();
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 50);
            }
            other => panic!("Expected Assistant, got {:?}", other),
        }
    }

    #[test]
    fn parse_tool_use_block() {
        let line = r#"{"type":"assistant","message":{"id":"msg_02","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[{"type":"tool_use","id":"tu_01","name":"Bash","input":{"command":"ls -la"}}],"stop_reason":"tool_use","usage":{"input_tokens":200,"output_tokens":80,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"parent_tool_use_id":null,"uuid":"uuid-3","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::Assistant(a) => {
                assert_eq!(a.message.content.len(), 1);
                match &a.message.content[0] {
                    ContentBlock::ToolUse { id, name, input } => {
                        assert_eq!(id, "tu_01");
                        assert_eq!(name, "Bash");
                        assert_eq!(input["command"], "ls -la");
                    }
                    other => panic!("Expected ToolUse block, got {:?}", other),
                }
            }
            other => panic!("Expected Assistant, got {:?}", other),
        }
    }

    #[test]
    fn parse_result_success() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"Done","duration_ms":5000,"duration_api_ms":3000,"num_turns":2,"total_cost_usd":0.05,"stop_reason":"end_turn","usage":{"input_tokens":500,"output_tokens":200,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"uuid":"uuid-4","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::Result(r) => {
                assert_eq!(r.subtype, "success");
                assert!(!r.is_error);
                assert_eq!(r.total_cost_usd, 0.05);
                assert_eq!(r.num_turns, 2);
                assert_eq!(r.duration_ms, 5000);
            }
            other => panic!("Expected Result, got {:?}", other),
        }
    }

    #[test]
    fn parse_result_error() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["something broke"],"duration_ms":1000,"duration_api_ms":500,"num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":100,"output_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"uuid":"uuid-5","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::Result(r) => {
                assert!(r.is_error);
                assert_eq!(r.subtype, "error_during_execution");
                let errors = r.errors.unwrap();
                assert_eq!(errors[0], "something broke");
            }
            other => panic!("Expected Result, got {:?}", other),
        }
    }

    #[test]
    fn parse_control_request_can_use_tool() {
        let line = r#"{"type":"control_request","request_id":"req-001","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"},"tool_use_id":"tu_02","description":"dangerous command"}}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::ControlRequest(cr) => {
                assert_eq!(cr.request_id, "req-001");
                let subtype = cr.request["subtype"].as_str().unwrap();
                assert_eq!(subtype, "can_use_tool");

                // Parse the inner payload
                let payload: CanUseToolRequest =
                    serde_json::from_value(cr.request).unwrap();
                assert_eq!(payload.tool_name, "Bash");
                assert_eq!(payload.input["command"], "rm -rf /");
            }
            other => panic!("Expected ControlRequest, got {:?}", other),
        }
    }

    #[test]
    fn parse_stream_event_text_delta() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}},"parent_tool_use_id":null,"uuid":"uuid-6","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::StreamEvent(se) => {
                let text = extract_stream_text(&se);
                assert_eq!(text, Some("Hello "));
            }
            other => panic!("Expected StreamEvent, got {:?}", other),
        }
    }

    #[test]
    fn parse_tool_progress() {
        let line = r#"{"type":"tool_progress","tool_use_id":"tu_03","tool_name":"Read","elapsed_time_seconds":2.5,"parent_tool_use_id":null,"uuid":"uuid-7","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::ToolProgress(tp) => {
                assert_eq!(tp.tool_name, "Read");
                assert_eq!(tp.tool_use_id, "tu_03");
                assert!((tp.elapsed_time_seconds - 2.5).abs() < 0.01);
            }
            other => panic!("Expected ToolProgress, got {:?}", other),
        }
    }

    #[test]
    fn parse_keep_alive() {
        let line = r#"{"type":"keep_alive"}"#;
        let msg = parse_incoming(line).unwrap();
        assert!(matches!(msg, IncomingMessage::KeepAlive));
    }

    #[test]
    fn parse_unknown_type() {
        let line = r#"{"type":"streamlined_text","text":"foo","session_id":"abc","uuid":"x"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::Unknown { msg_type } => {
                assert_eq!(msg_type, "streamlined_text");
            }
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn serialize_user_message() {
        let msg = UserMessage::new("Hello".to_string(), "sess123".to_string());
        let json = to_ndjson(&msg).unwrap();
        let v: Value = serde_json::from_str(json.trim()).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "Hello");
        assert_eq!(v["session_id"], "sess123");
        assert!(v["parent_tool_use_id"].is_null());
    }

    #[test]
    fn serialize_control_response_allow() {
        let input = serde_json::json!({"command": "ls"});
        let msg = ControlResponse::allow("req-001".to_string(), input);
        let json = to_ndjson(&msg).unwrap();
        let v: Value = serde_json::from_str(json.trim()).unwrap();
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["subtype"], "success");
        assert_eq!(v["response"]["request_id"], "req-001");
        assert_eq!(v["response"]["response"]["behavior"], "allow");
        assert_eq!(v["response"]["response"]["updatedInput"]["command"], "ls");
    }

    #[test]
    fn serialize_control_response_deny() {
        let msg = ControlResponse::deny("req-002".to_string(), "not allowed".to_string());
        let json = to_ndjson(&msg).unwrap();
        let v: Value = serde_json::from_str(json.trim()).unwrap();
        assert_eq!(v["response"]["response"]["behavior"], "deny");
        assert_eq!(v["response"]["response"]["message"], "not allowed");
    }

    #[test]
    fn serialize_interrupt() {
        let msg = OutgoingControlRequest::interrupt("req-003".to_string());
        let json = to_ndjson(&msg).unwrap();
        let v: Value = serde_json::from_str(json.trim()).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request"]["subtype"], "interrupt");
        assert_eq!(v["request_id"], "req-003");
    }

    #[test]
    fn stream_event_thinking_delta() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}},"parent_tool_use_id":null,"uuid":"uuid-8","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::StreamEvent(se) => {
                let thinking = extract_stream_thinking(&se);
                assert_eq!(thinking, Some("Let me think..."));
                assert!(extract_stream_text(&se).is_none());
            }
            other => panic!("Expected StreamEvent, got {:?}", other),
        }
    }

    #[test]
    fn stream_event_block_start() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_99","name":"Bash","input":{}}},"parent_tool_use_id":null,"uuid":"uuid-9","session_id":"abc123"}"#;
        let msg = parse_incoming(line).unwrap();
        match msg {
            IncomingMessage::StreamEvent(se) => {
                assert_eq!(stream_event_block_type(&se), Some("tool_use"));
            }
            other => panic!("Expected StreamEvent, got {:?}", other),
        }
    }
}
