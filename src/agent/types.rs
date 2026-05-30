use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentType {
    ClaudeCode,
    Codex,
    OpenCode,
}

impl AgentType {
    pub fn command(&self) -> &'static str {
        match self {
            AgentType::ClaudeCode => "claude",
            AgentType::Codex => "codex",
            AgentType::OpenCode => "opencode",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AgentType::ClaudeCode => "claude",
            AgentType::Codex => "codex",
            AgentType::OpenCode => "opencode",
        }
    }

    pub fn all() -> &'static [AgentType] {
        &[AgentType::ClaudeCode, AgentType::Codex, AgentType::OpenCode]
    }

    pub fn output_buffer_size(&self) -> usize {
        match self {
            AgentType::OpenCode => 512 * 1024,
            _ => 128 * 1024,
        }
    }
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

pub struct AgentConfig {
    pub agent_type: AgentType,
    pub repo_path: PathBuf,
    pub prompt: Option<String>,
}
