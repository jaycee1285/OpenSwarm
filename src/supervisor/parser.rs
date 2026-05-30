//! Event parser for detecting structured events from agent PTY output.
//!
//! Parses tool usage, thinking indicators, and other events from the raw
//! terminal output of AI coding agents.

use std::time::Instant;

use crate::agent::types::AgentType;
use crate::ipc::proto::AgentEventType;

/// Parser state for detecting events in agent output.
pub struct EventParser {
    agent_type: AgentType,
    buffer: String,
    current_tool: Option<(String, Instant)>,
    last_thinking: Option<Instant>,
}

impl EventParser {
    pub fn new(agent_type: AgentType) -> Self {
        Self {
            agent_type,
            buffer: String::new(),
            current_tool: None,
            last_thinking: None,
        }
    }

    /// Feed output bytes and return any detected events.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<AgentEventType> {
        let mut events = Vec::new();

        let Ok(text) = std::str::from_utf8(bytes) else {
            return events;
        };

        self.buffer.push_str(text);

        match self.agent_type {
            AgentType::ClaudeCode => {
                events.extend(self.parse_claude_code());
            }
            AgentType::Codex => {
                events.extend(self.parse_codex());
            }
            AgentType::OpenCode => {
                events.extend(self.parse_opencode());
            }
        }

        // Trim buffer to prevent unbounded growth
        if self.buffer.len() > 8192 {
            self.buffer.drain(..self.buffer.len() - 4096);
        }

        events
    }

    fn parse_claude_code(&mut self) -> Vec<AgentEventType> {
        let mut events = Vec::new();

        // Detect thinking (spinner characters)
        let spinners = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        if spinners.iter().any(|&c| self.buffer.contains(c)) {
            let now = Instant::now();
            let should_emit = self
                .last_thinking
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true);
            if should_emit {
                self.last_thinking = Some(now);
                events.push(AgentEventType::Thinking);
            }
        }

        // Detect tool start patterns
        // Claude Code shows tool names in output like "Read(" or "Edit("
        let tool_patterns = [
            ("Read", "Read("),
            ("Edit", "Edit("),
            ("Write", "Write("),
            ("Bash", "Bash("),
            ("Glob", "Glob("),
            ("Grep", "Grep("),
            ("Task", "Task("),
            ("WebFetch", "WebFetch("),
            ("WebSearch", "WebSearch("),
        ];

        for (tool_name, pattern) in tool_patterns {
            if self.buffer.contains(pattern) {
                if self.current_tool.as_ref().map(|(t, _)| t.as_str()) != Some(tool_name) {
                    // End previous tool if any
                    if let Some((prev_tool, start)) = self.current_tool.take() {
                        let duration = Instant::now().duration_since(start).as_millis() as u64;
                        events.push(AgentEventType::ToolEnd {
                            tool_name: prev_tool,
                            success: true,
                            duration_ms: duration,
                        });
                    }
                    // Start new tool
                    events.push(AgentEventType::ToolStart {
                        tool_name: tool_name.to_string(),
                    });
                    self.current_tool = Some((tool_name.to_string(), Instant::now()));
                }
            }
        }

        // Detect cost updates (Claude shows "Cost: $X.XX" or similar)
        if let Some(cost) = self.extract_cost() {
            events.push(AgentEventType::CostUpdate { total_dollars: cost });
        }

        // Clear processed patterns from buffer
        self.buffer.clear();

        events
    }

    fn parse_codex(&mut self) -> Vec<AgentEventType> {
        let mut events = Vec::new();

        // Codex has different output patterns
        // Look for common indicators
        if self.buffer.contains("thinking") || self.buffer.contains("...") {
            events.push(AgentEventType::Thinking);
        }

        self.buffer.clear();
        events
    }

    fn parse_opencode(&mut self) -> Vec<AgentEventType> {
        let mut events = Vec::new();

        // OpenCode patterns (similar to Claude Code since it's based on it)
        let spinners = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        if spinners.iter().any(|&c| self.buffer.contains(c)) {
            events.push(AgentEventType::Thinking);
        }

        self.buffer.clear();
        events
    }

    fn extract_cost(&self) -> Option<f64> {
        // Look for patterns like "Cost: $0.12" or "Total: $1.23"
        for pattern in ["Cost: $", "Total: $", "cost: $"] {
            if let Some(pos) = self.buffer.find(pattern) {
                let start = pos + pattern.len();
                let rest = &self.buffer[start..];
                let end = rest
                    .find(|c: char| !c.is_ascii_digit() && c != '.')
                    .unwrap_or(rest.len());
                if let Ok(cost) = rest[..end].parse::<f64>() {
                    return Some(cost);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_tool_start() {
        let mut parser = EventParser::new(AgentType::ClaudeCode);
        let events = parser.feed(b"Using Read(file.txt)");
        assert!(events.iter().any(|e| matches!(e, AgentEventType::ToolStart { tool_name } if tool_name == "Read")));
    }

    #[test]
    fn test_detect_thinking() {
        let mut parser = EventParser::new(AgentType::ClaudeCode);
        let events = parser.feed("⠋ Thinking...".as_bytes());
        assert!(events.iter().any(|e| matches!(e, AgentEventType::Thinking)));
    }
}
