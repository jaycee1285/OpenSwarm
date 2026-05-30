use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::status::AgentStatus;
use crate::agent::types::AgentType;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: u32,
    #[serde(default)]
    pub parent_id: Option<u32>,
    pub agent_type: AgentType,
    pub repo_path: String,
    pub repo_name: String,
    pub status: AgentStatus,
    /// Claude Code session UUID (used for `--resume`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Codex thread UUID (used for `thread/resume`).  Separate from
    /// session_id so clients don't have to check agent_type to interpret
    /// the resumption handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentSessionInfo {
    pub id: u32,
    pub repo_name: String,
    pub repo_path: String,
    pub agent_type: AgentType,
    pub can_resume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_handle: Option<String>,
    /// Spawn date in local MM/DD format.
    pub date_mmdd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub repo_name: String,
    pub repo_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub agent_type: AgentType,
    pub default_model: String,
    pub options: Vec<ModelOption>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    SpawnAgent {
        agent_type: AgentType,
        repo_path: String,
        prompt: Option<String>,
        #[serde(default)]
        parent_id: Option<u32>,
        /// Model override (e.g. "opus", "gpt-5-codex", "openrouter/moonshotai/kimi-k2-thinking")
        #[serde(default)]
        model: Option<String>,
    },
    KillAgent {
        agent_id: u32,
    },
    Input {
        agent_id: u32,
        bytes: Vec<u8>,
    },
    ResizeAgent {
        agent_id: u32,
        rows: u16,
        cols: u16,
    },
    SetOutputPaused {
        agent_id: u32,
        paused: bool,
    },
    /// Configure WebSocket remote access.
    SetWsConfig {
        enabled: bool,
        password: String,
    },
    /// Request current WebSocket status.
    GetWsStatus,
    /// Resume a previously exited agent.
    ResumeAgent {
        agent_id: u32,
    },
    /// Resume a portable exported session by its native agent handle.
    ResumeExportedSession {
        agent_type: AgentType,
        repo_path: String,
        session_handle: String,
    },
    /// Send a prompt to a Claude WS agent (multi-turn).
    SendPrompt {
        agent_id: u32,
        prompt: String,
    },
    /// Respond to a tool approval request from a Claude WS agent.
    ToolApprovalResponse {
        agent_id: u32,
        request_id: String,
        approved: bool,
        #[serde(default)]
        updated_input: Option<Value>,
    },
    /// Interrupt a Claude WS agent's current turn.
    Interrupt {
        agent_id: u32,
    },
    /// Reply to a structured question request (OpenCode).
    QuestionResponse {
        agent_id: u32,
        request_id: String,
        answers: Vec<Vec<String>>,
        #[serde(default)]
        rejected: bool,
    },
    /// Request a usage status refresh.
    RefreshUsage,
    /// Acknowledge receipt of server messages up to this sequence number
    /// (WebSocket transport only).
    Ack {
        last_seq: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEventType {
    ToolStart { tool_name: String },
    ToolEnd { tool_name: String, success: bool, duration_ms: u64 },
    TokenUsage { input_tokens: u64, output_tokens: u64 },
    CostUpdate { total_dollars: f64 },
    Thinking,
    WaitingForInput,
    Error { message: String },
    ParentExited { parent_id: u32 },
    SessionInit { model: String, session_id: String },
    QueryComplete { num_turns: u64, duration_ms: u64, is_error: bool },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome {
        protocol: u32,
        build_id: u64,
    },
    ModelCatalog {
        catalogs: Vec<ModelCatalogEntry>,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    RepoInventory {
        repos: Vec<RepoInfo>,
    },
    RecentSessions {
        sessions: Vec<RecentSessionInfo>,
    },
    AgentOutput {
        agent_id: u32,
        bytes: Vec<u8>,
    },
    AgentStatus {
        agent_id: u32,
        status: AgentStatus,
    },
    AgentEvent {
        agent_id: u32,
        timestamp: u64,
        event: AgentEventType,
    },
    /// WebSocket server status.
    WsStatus {
        enabled: bool,
        connected_peers: Vec<String>,
    },
    /// Tool approval request from a Claude WS agent.
    ToolApprovalRequest {
        agent_id: u32,
        request_id: String,
        tool_name: String,
        tool_input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// Structured question request (OpenCode).
    QuestionRequest {
        agent_id: u32,
        request_id: String,
        questions: Value,
    },
    /// Global usage status from polling `claude /usage`.
    UsageStatus {
        raw_output: String,
        /// Real percentages from `/usage` probe
        #[serde(skip_serializing_if = "Option::is_none")]
        session_percent: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_reset: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        week_all_percent: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        week_all_reset: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        week_sonnet_percent: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        week_sonnet_reset: Option<String>,
        /// File-based fallback fields
        #[serde(skip_serializing_if = "Option::is_none")]
        session_messages: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_limit: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        daily_messages: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        weekly_messages: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        messages_used: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        messages_limit: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        plan_tier: Option<String>,
        /// Codex account limits (from account/rateLimits/read + updated).
        #[serde(skip_serializing_if = "Option::is_none")]
        codex_five_hour_percent: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        codex_five_hour_reset: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        codex_weekly_percent: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        codex_weekly_reset: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_spawn_agent_back_compat_optional_fields_default() {
        let value = json!({
            "SpawnAgent": {
                "agent_type": "Codex",
                "repo_path": "/tmp/repo",
                "prompt": "hello"
            }
        });

        let parsed: ClientMessage = serde_json::from_value(value).expect("parse spawn");
        match parsed {
            ClientMessage::SpawnAgent {
                parent_id, model, ..
            } => {
                assert_eq!(parent_id, None);
                assert_eq!(model, None);
            }
            _ => panic!("expected SpawnAgent"),
        }
    }

    #[test]
    fn client_tool_approval_back_compat_updated_input_default() {
        let value = json!({
            "ToolApprovalResponse": {
                "agent_id": 7,
                "request_id": "req-1",
                "approved": true
            }
        });

        let parsed: ClientMessage = serde_json::from_value(value).expect("parse tool approval");
        match parsed {
            ClientMessage::ToolApprovalResponse {
                updated_input, ..
            } => {
                assert_eq!(updated_input, None);
            }
            _ => panic!("expected ToolApprovalResponse"),
        }
    }

    #[test]
    fn server_agent_info_omits_resume_handles_when_none() {
        let info = AgentInfo {
            id: 1,
            parent_id: None,
            agent_type: AgentType::ClaudeCode,
            repo_path: "/tmp/repo".to_string(),
            repo_name: "repo".to_string(),
            status: AgentStatus::Running,
            session_id: None,
            thread_id: None,
        };
        let msg = ServerMessage::AgentList { agents: vec![info] };

        let value = serde_json::to_value(msg).expect("serialize");
        let agent = value
            .get("AgentList")
            .and_then(|v| v.get("agents"))
            .and_then(|v| v.get(0))
            .expect("first agent");
        assert!(agent.get("session_id").is_none());
        assert!(agent.get("thread_id").is_none());
    }

    #[test]
    fn usage_status_round_trip_preserves_payload() {
        let msg = ServerMessage::UsageStatus {
            raw_output: "raw".to_string(),
            session_percent: Some(12),
            session_reset: Some("Resets in 4h".to_string()),
            week_all_percent: Some(55),
            week_all_reset: Some("Resets Sun".to_string()),
            week_sonnet_percent: None,
            week_sonnet_reset: None,
            session_messages: Some(11),
            session_limit: Some(100),
            daily_messages: Some(22),
            weekly_messages: Some(44),
            messages_used: Some(66),
            messages_limit: Some(88),
            plan_tier: Some("pro".to_string()),
            codex_five_hour_percent: Some(33),
            codex_five_hour_reset: Some("Resets soon".to_string()),
            codex_weekly_percent: Some(77),
            codex_weekly_reset: Some("Resets later".to_string()),
        };

        let encoded = serde_json::to_string(&msg).expect("encode usage");
        let decoded: ServerMessage = serde_json::from_str(&encoded).expect("decode usage");
        match decoded {
            ServerMessage::UsageStatus {
                raw_output,
                session_percent,
                codex_weekly_percent,
                ..
            } => {
                assert_eq!(raw_output, "raw");
                assert_eq!(session_percent, Some(12));
                assert_eq!(codex_weekly_percent, Some(77));
            }
            _ => panic!("expected UsageStatus"),
        }
    }
}
