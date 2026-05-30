//! ANSI renderer — converts structured Claude protocol messages into
//! styled terminal bytes for VTE `feed()` and xterm.js `write()`.
//!
//! All output is plain ANSI escape sequences targeting the Flexoki Light
//! 16-color palette configured in `config.rs`.  No PTY needed — the bytes
//! are fed directly into the terminal widget.

use serde_json::Value;

use super::claude_proto::{
    AssistantMessage, CanUseToolRequest, ContentBlock, ResultMessage, StreamEvent, SystemInit,
    ToolProgress, extract_stream_text, extract_stream_thinking,
};

// ANSI escape helpers — use the 16-color palette indices matching Flexoki Light.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";

// Foreground colors (standard 16-color)
const FG_RED: &str = "\x1b[31m";
const FG_GREEN: &str = "\x1b[32m";
const FG_YELLOW: &str = "\x1b[33m";
const FG_BLUE: &str = "\x1b[34m";
const FG_MAGENTA: &str = "\x1b[35m";
const FG_CYAN: &str = "\x1b[36m";
const FG_BRIGHT_BLACK: &str = "\x1b[90m";

/// Render `system/init` — session startup header.
pub fn render_init(init: &SystemInit) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&format!(
        "\r\n{BOLD}{FG_CYAN}━━━ Claude Code Session ━━━{RESET}\r\n"
    ));
    out.push_str(&format!(
        "{FG_CYAN}  model:   {RESET}{}\r\n",
        init.model
    ));
    out.push_str(&format!(
        "{FG_CYAN}  session: {RESET}{}\r\n",
        init.session_id
    ));
    if !init.tools.is_empty() {
        let tool_list = if init.tools.len() <= 8 {
            init.tools.join(", ")
        } else {
            let shown: Vec<&str> = init.tools.iter().take(8).map(|s| s.as_str()).collect();
            format!("{} (+{} more)", shown.join(", "), init.tools.len() - 8)
        };
        out.push_str(&format!("{FG_CYAN}  tools:   {RESET}{tool_list}\r\n"));
    }
    out.push_str(&format!("{BOLD}{FG_CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━{RESET}\r\n\r\n"));
    out.into_bytes()
}

/// Render an outgoing user prompt.
pub fn render_user_prompt(prompt: &str) -> Vec<u8> {
    format!("{BOLD}{FG_GREEN}> {RESET}{FG_GREEN}{prompt}{RESET}\r\n\r\n").into_bytes()
}

/// Render a `stream_event` text delta — raw text for ticker effect.
pub fn render_stream_delta(event: &StreamEvent) -> Option<Vec<u8>> {
    if let Some(text) = extract_stream_text(event) {
        // Replace bare \n with \r\n for terminal display
        let fixed = text.replace('\n', "\r\n");
        return Some(fixed.into_bytes());
    }
    if let Some(thinking) = extract_stream_thinking(event) {
        let fixed = thinking.replace('\n', "\r\n");
        return Some(
            format!("{DIM}{ITALIC}{fixed}{RESET}").into_bytes(),
        );
    }
    None
}

/// Render the non-text content blocks from an `assistant` message.
///
/// Text blocks are skipped because they were already streamed via
/// `stream_event` deltas.  This renders tool_use, tool_result, and
/// thinking blocks only.
pub fn render_assistant_blocks(msg: &AssistantMessage) -> Vec<u8> {
    let mut out = Vec::new();
    for block in &msg.message.content {
        match block {
            ContentBlock::Text { text } => {
                // Render the full text block — stream_event deltas may not
                // be available over the WS transport, so this is the primary
                // text output path.
                if !text.is_empty() {
                    let fixed = text.replace('\n', "\r\n");
                    out.extend_from_slice(fixed.as_bytes());
                    out.extend_from_slice(b"\r\n");
                }
            }
            ContentBlock::ToolUse { name, input, .. } => {
                out.extend_from_slice(&render_tool_use(name, input));
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                out.extend_from_slice(&render_tool_result(content, *is_error));
            }
            ContentBlock::Thinking { thinking, .. } => {
                // Full thinking block (if not already streamed)
                if !thinking.is_empty() {
                    let fixed = thinking.replace('\n', "\r\n");
                    out.extend_from_slice(
                        format!("{DIM}{ITALIC}[thinking] {fixed}{RESET}\r\n").as_bytes(),
                    );
                }
            }
        }
    }
    out
}

/// Render a tool_use block with condensed input display.
fn render_tool_use(name: &str, input: &Value) -> Vec<u8> {
    let summary = format_tool_input(name, input);
    format!("\r\n{BOLD}{FG_YELLOW}{name}{RESET}{FG_YELLOW}({summary}){RESET}\r\n").into_bytes()
}

/// Format tool input in a condensed way, tailored per tool.
fn format_tool_input(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|cmd| truncate(cmd, 120))
            .unwrap_or_else(|| format_value_brief(input)),

        "Read" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| truncate(p, 120))
            .unwrap_or_else(|| format_value_brief(input)),

        "Write" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| truncate(p, 120))
            .unwrap_or_else(|| format_value_brief(input)),

        "Edit" => {
            let file = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            truncate(file, 120)
        }

        "Grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| truncate(p, 80))
            .unwrap_or_else(|| format_value_brief(input)),

        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| truncate(p, 80))
            .unwrap_or_else(|| format_value_brief(input)),

        "Task" => input
            .get("description")
            .and_then(|v| v.as_str())
            .map(|d| truncate(d, 80))
            .unwrap_or_else(|| format_value_brief(input)),

        _ => format_value_brief(input),
    }
}

/// Render a tool_result block.
fn render_tool_result(content: &Value, is_error: bool) -> Vec<u8> {
    let (color, label) = if is_error {
        (FG_RED, "ERROR")
    } else {
        (FG_BRIGHT_BLACK, "")
    };

    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(arr) => {
            // Extract text from content block array
            arr.iter()
                .filter_map(|b| {
                    if b.get("type")?.as_str()? == "text" {
                        b.get("text")?.as_str().map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => serde_json::to_string(content).unwrap_or_default(),
    };

    if text.is_empty() {
        return Vec::new();
    }

    let mut out = String::new();

    if is_error {
        out.push_str(&format!("{BOLD}{color}  [{label}]{RESET} "));
    }

    // Truncate to 20 lines, indent each line
    let lines: Vec<&str> = text.lines().collect();
    let truncated = lines.len() > 20;
    let show_lines = if truncated { &lines[..20] } else { &lines };

    for line in show_lines {
        let fixed = line.replace('\t', "    ");
        out.push_str(&format!("{DIM}{color}  {fixed}{RESET}\r\n"));
    }
    if truncated {
        out.push_str(&format!(
            "{DIM}{FG_BRIGHT_BLACK}  ... ({} more lines){RESET}\r\n",
            lines.len() - 20
        ));
    }

    out.into_bytes()
}

/// Render a tool_progress heartbeat.
pub fn render_tool_progress(progress: &ToolProgress) -> Vec<u8> {
    let secs = progress.elapsed_time_seconds as u64;
    format!(
        "{DIM}{FG_BRIGHT_BLACK}  ⏳ {name} ({secs}s){RESET}\r",
        name = progress.tool_name,
    )
    .into_bytes()
}

/// Render a `can_use_tool` approval request.
pub fn render_approval_request(req: &CanUseToolRequest) -> Vec<u8> {
    let summary = format_tool_input(&req.tool_name, &req.input);
    format!(
        "\r\n{BOLD}{FG_MAGENTA}[APPROVAL REQUIRED]{RESET} {FG_YELLOW}{name}{RESET}({summary})\r\n",
        name = req.tool_name,
    )
    .into_bytes()
}

/// Render approval granted.
pub fn render_approval_granted(tool_name: &str) -> Vec<u8> {
    format!("{FG_GREEN}  ✓ approved{RESET} {tool_name}\r\n").into_bytes()
}

/// Render approval denied.
pub fn render_approval_denied(tool_name: &str, reason: &str) -> Vec<u8> {
    format!("{FG_RED}  ✗ denied{RESET} {tool_name}: {reason}\r\n").into_bytes()
}

/// Render `result` — query complete summary.
pub fn render_result(result: &ResultMessage) -> Vec<u8> {
    let duration_s = result.duration_ms as f64 / 1000.0;

    if result.is_error {
        let errors = result
            .errors
            .as_ref()
            .map(|e| e.join("; "))
            .unwrap_or_else(|| result.subtype.clone());
        format!(
            "\r\n{BOLD}{FG_RED}[ERROR] {errors}{RESET}\r\n\
             {FG_RED}  cost=${cost:.4} turns={turns} {dur:.1}s{RESET}\r\n\r\n",
            cost = result.total_cost_usd,
            turns = result.num_turns,
            dur = duration_s,
        )
        .into_bytes()
    } else {
        format!(
            "\r\n{BOLD}{FG_CYAN}[QUERY COMPLETE]{RESET}\
             {FG_CYAN} cost=${cost:.4} turns={turns} {dur:.1}s{RESET}\r\n\r\n",
            cost = result.total_cost_usd,
            turns = result.num_turns,
            dur = duration_s,
        )
        .into_bytes()
    }
}

/// Render a status change (e.g. compacting).
pub fn render_status(status: Option<&str>) -> Vec<u8> {
    match status {
        Some("compacting") => {
            format!("{DIM}{FG_BLUE}[compacting context...]{RESET}\r\n").into_bytes()
        }
        Some(other) => format!("{DIM}{FG_BLUE}[{other}]{RESET}\r\n").into_bytes(),
        None => format!("{DIM}{FG_BLUE}[compaction complete]{RESET}\r\n").into_bytes(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
    fn render_init_output() {
        let init = SystemInit {
            session_id: "test-session".to_string(),
            tools: vec!["Bash".to_string(), "Read".to_string()],
            model: "claude-sonnet-4-5-20250929".to_string(),
            cwd: "/home/user".to_string(),
            claude_code_version: None,
            permission_mode: None,
            uuid: None,
        };
        let bytes = render_init(&init);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("Claude Code Session"));
        assert!(text.contains("claude-sonnet-4-5-20250929"));
        assert!(text.contains("test-session"));
        assert!(text.contains("Bash, Read"));
    }

    #[test]
    fn render_user_prompt_output() {
        let bytes = render_user_prompt("What files are here?");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("> "));
        assert!(text.contains("What files are here?"));
    }

    #[test]
    fn render_stream_text_delta() {
        let event = StreamEvent {
            event: serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "Hello world" }
            }),
            parent_tool_use_id: None,
            uuid: None,
            session_id: None,
        };
        let bytes = render_stream_delta(&event).unwrap();
        assert_eq!(bytes, b"Hello world");
    }

    #[test]
    fn render_stream_newlines_converted() {
        let event = StreamEvent {
            event: serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "line1\nline2" }
            }),
            parent_tool_use_id: None,
            uuid: None,
            session_id: None,
        };
        let bytes = render_stream_delta(&event).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("line1\r\nline2"));
    }

    #[test]
    fn render_tool_use_bash() {
        let bytes = render_tool_use(
            "Bash",
            &serde_json::json!({"command": "git status"}),
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("Bash"));
        assert!(text.contains("git status"));
    }

    #[test]
    fn render_tool_use_read() {
        let bytes = render_tool_use(
            "Read",
            &serde_json::json!({"file_path": "/home/user/foo.rs"}),
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("Read"));
        assert!(text.contains("/home/user/foo.rs"));
    }

    #[test]
    fn render_tool_result_truncation() {
        let long_content: String = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let bytes = render_tool_result(&Value::String(long_content), false);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("10 more lines"));
    }

    #[test]
    fn render_tool_result_error() {
        let bytes =
            render_tool_result(&Value::String("permission denied".to_string()), true);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("ERROR"));
        assert!(text.contains("permission denied"));
    }

    #[test]
    fn render_result_success() {
        let result = ResultMessage {
            subtype: "success".to_string(),
            is_error: false,
            result: Some("Done".to_string()),
            errors: None,
            duration_ms: 12500,
            duration_api_ms: 10000,
            num_turns: 3,
            total_cost_usd: 0.042,
            usage: None,
            model_usage: None,
            stop_reason: None,
            uuid: None,
            session_id: None,
        };
        let bytes = render_result(&result);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("QUERY COMPLETE"));
        assert!(text.contains("$0.0420"));
        assert!(text.contains("turns=3"));
        assert!(text.contains("12.5s"));
    }

    #[test]
    fn render_result_error() {
        let result = ResultMessage {
            subtype: "error_during_execution".to_string(),
            is_error: true,
            result: None,
            errors: Some(vec!["something went wrong".to_string()]),
            duration_ms: 1000,
            duration_api_ms: 500,
            num_turns: 1,
            total_cost_usd: 0.01,
            usage: None,
            model_usage: None,
            stop_reason: None,
            uuid: None,
            session_id: None,
        };
        let bytes = render_result(&result);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("ERROR"));
        assert!(text.contains("something went wrong"));
    }

    #[test]
    fn render_approval_request_output() {
        let req = CanUseToolRequest {
            tool_name: "Bash".to_string(),
            input: serde_json::json!({"command": "rm -rf /tmp/stuff"}),
            tool_use_id: None,
            description: None,
            agent_id: None,
        };
        let bytes = render_approval_request(&req);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("APPROVAL REQUIRED"));
        assert!(text.contains("Bash"));
        assert!(text.contains("rm -rf /tmp/stuff"));
    }

    #[test]
    fn render_status_compacting() {
        let bytes = render_status(Some("compacting"));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("compacting context"));
    }

    #[test]
    fn render_status_done() {
        let bytes = render_status(None);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("compaction complete"));
    }

    #[test]
    fn render_tool_progress_output() {
        let tp = ToolProgress {
            tool_use_id: "tu_01".to_string(),
            tool_name: "Bash".to_string(),
            elapsed_time_seconds: 5.2,
            parent_tool_use_id: None,
            uuid: None,
            session_id: None,
        };
        let bytes = render_tool_progress(&tp);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("Bash"));
        assert!(text.contains("5s"));
    }
}
