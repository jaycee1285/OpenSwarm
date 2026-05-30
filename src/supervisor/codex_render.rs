//! ANSI renderer for Codex app-server events.
//!
//! Mirrors the visual style of `ansi_render.rs` (Claude WS renderer)
//! using the same Flexoki Light 16-color palette.

use serde_json::Value;

// ANSI escape helpers — same palette as ansi_render.rs
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";

const FG_RED: &str = "\x1b[31m";
const FG_GREEN: &str = "\x1b[32m";
const FG_YELLOW: &str = "\x1b[33m";
const FG_BLUE: &str = "\x1b[34m";
const FG_MAGENTA: &str = "\x1b[35m";
const FG_CYAN: &str = "\x1b[36m";
const FG_BRIGHT_BLACK: &str = "\x1b[90m";

/// Render session startup header (thread created).
pub fn render_session_header(thread_id: &str) -> Vec<u8> {
    format!(
        "\r\n{BOLD}{FG_CYAN}━━━ Codex Session ━━━{RESET}\r\n\
         {FG_CYAN}  thread: {RESET}{thread_id}\r\n\
         {BOLD}{FG_CYAN}━━━━━━━━━━━━━━━━━━━━━{RESET}\r\n\r\n"
    )
    .into_bytes()
}

/// Render an outgoing user prompt.
pub fn render_user_prompt(prompt: &str) -> Vec<u8> {
    format!("{BOLD}{FG_GREEN}> {RESET}{FG_GREEN}{prompt}{RESET}\r\n\r\n").into_bytes()
}

/// Render a streaming text delta (agent message).
/// Converts bare \n to \r\n for terminal display.
pub fn render_text_delta(text: &str) -> Vec<u8> {
    text.replace('\n', "\r\n").into_bytes()
}

/// Render a reasoning/thinking delta.
pub fn render_thinking(text: &str) -> Vec<u8> {
    let fixed = text.replace('\n', "\r\n");
    format!("{DIM}{ITALIC}{fixed}{RESET}").into_bytes()
}

/// Render a command execution output delta.
pub fn render_command_output(text: &str) -> Vec<u8> {
    let fixed = text.replace('\n', "\r\n");
    format!("{DIM}{fixed}{RESET}").into_bytes()
}

/// Render a file change output delta.
pub fn render_file_change(text: &str) -> Vec<u8> {
    let fixed = text.replace('\n', "\r\n");
    format!("{FG_CYAN}{fixed}{RESET}").into_bytes()
}

/// Render a tool start header (item/started for non-agentMessage items).
pub fn render_tool_header(tool_name: &str) -> Vec<u8> {
    format!("\r\n{BOLD}{FG_YELLOW}{tool_name}{RESET}\r\n").into_bytes()
}

/// Render turn completion separator.
pub fn render_turn_complete() -> Vec<u8> {
    format!("\r\n{BOLD}{FG_CYAN}[TURN COMPLETE]{RESET}\r\n\r\n").into_bytes()
}

/// Render context compaction notice.
pub fn render_compacted() -> Vec<u8> {
    format!("{DIM}{FG_BLUE}[context compacted]{RESET}\r\n").into_bytes()
}

/// Render a tool approval request.
pub fn render_approval_request(tool_name: &str, params: &Value) -> Vec<u8> {
    let summary = format_tool_params(tool_name, params);
    format!(
        "\r\n{BOLD}{FG_MAGENTA}[APPROVAL REQUIRED]{RESET} {FG_YELLOW}{tool_name}{RESET}({summary})\r\n"
    )
    .into_bytes()
}

/// Render approval granted.
pub fn render_approval_granted(tool_name: &str) -> Vec<u8> {
    format!("{FG_GREEN}  ✓ approved{RESET} {tool_name}\r\n").into_bytes()
}

/// Render approval denied.
pub fn render_approval_denied(tool_name: &str) -> Vec<u8> {
    format!("{FG_RED}  ✗ denied{RESET} {tool_name}\r\n").into_bytes()
}

/// Render an error message.
pub fn render_error(message: &str, will_retry: bool) -> Vec<u8> {
    let suffix = if will_retry { " (retrying...)" } else { "" };
    format!("{BOLD}{FG_RED}Error: {message}{suffix}{RESET}\r\n").into_bytes()
}

/// Render an agent message block separator (newline after completed agentMessage).
pub fn render_message_separator() -> Vec<u8> {
    b"\r\n".to_vec()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format tool parameters in a condensed way, matching ansi_render patterns.
fn format_tool_params(tool_name: &str, params: &Value) -> String {
    // Codex uses different tool naming than Claude, but many overlap
    match tool_name {
        "shell" | "Bash" | "commandExecution" => params
            .get("command")
            .or_else(|| params.get("cmd"))
            .and_then(|v| v.as_str())
            .map(|cmd| truncate(cmd, 120))
            .unwrap_or_else(|| format_value_brief(params)),

        "fileRead" | "Read" => params
            .get("file_path")
            .or_else(|| params.get("path"))
            .and_then(|v| v.as_str())
            .map(|p| truncate(p, 120))
            .unwrap_or_else(|| format_value_brief(params)),

        "fileWrite" | "Write" | "fileChange" => params
            .get("file_path")
            .or_else(|| params.get("path"))
            .and_then(|v| v.as_str())
            .map(|p| truncate(p, 120))
            .unwrap_or_else(|| format_value_brief(params)),

        _ => format_value_brief(params),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn format_value_brief(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    truncate(&s, 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_header_contains_thread_id() {
        let bytes = render_session_header("thread-abc-123");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("Codex Session"));
        assert!(text.contains("thread-abc-123"));
    }

    #[test]
    fn user_prompt_formatting() {
        let bytes = render_user_prompt("fix the bug");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("> "));
        assert!(text.contains("fix the bug"));
    }

    #[test]
    fn text_delta_newline_conversion() {
        let bytes = render_text_delta("line1\nline2");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("line1\r\nline2"));
    }

    #[test]
    fn thinking_is_dim_italic() {
        let bytes = render_thinking("considering options");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(DIM));
        assert!(text.contains(ITALIC));
        assert!(text.contains("considering options"));
    }

    #[test]
    fn approval_request_formatting() {
        let bytes = render_approval_request(
            "shell",
            &serde_json::json!({"command": "npm install"}),
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("APPROVAL REQUIRED"));
        assert!(text.contains("shell"));
        assert!(text.contains("npm install"));
    }

    #[test]
    fn error_with_retry() {
        let bytes = render_error("rate limited", true);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("rate limited"));
        assert!(text.contains("retrying"));
    }

    #[test]
    fn error_without_retry() {
        let bytes = render_error("fatal error", false);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("fatal error"));
        assert!(!text.contains("retrying"));
    }
}
