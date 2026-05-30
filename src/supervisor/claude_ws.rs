//! Claude Code WebSocket driver.
//!
//! Spawns `claude --sdk-url ws://127.0.0.1:{port}` and acts as a WS server
//! that the CLI connects to.  Parses the NDJSON message stream and produces
//! both ANSI-formatted output (for VTE `feed()`) and structured `AgentEvent`
//! messages (for the dashboard and mobile).
//!
//! The driver runs a single-threaded poll loop on a dedicated thread:
//! read from the WebSocket with a short timeout, then drain any pending
//! commands from the supervisor.  No async runtime, no shared mutex on
//! the WebSocket — avoids the deadlock where the reader thread held the
//! mutex during blocking `ws.read()` while the event loop needed the
//! same mutex to send outgoing messages.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tungstenite::protocol::Message;
use tungstenite::handshake::server::{Request, Response};

use serde_json::{json, Value};

use crate::agent::status::AgentStatus;
use crate::ipc::proto::{AgentEventType, ServerMessage};

use super::ansi_render;
use super::claude_proto::{
    self, CanUseToolRequest, ControlResponse, IncomingMessage, OutgoingControlRequest,
    UserMessage, to_ndjson,
};
use super::server::{
    ClaudeDriverCommand, SupervisorState,
    append_output, broadcast, set_status,
};
use super::md_log::{ConversationMdLog, MdLogMode};
use super::session_artifact::SessionArtifact;

// ---------------------------------------------------------------------------
// Transcript — JSONL archive of all WS messages
// ---------------------------------------------------------------------------

struct Transcript {
    writer: BufWriter<fs::File>,
    verbose: bool,
    pending_agent: String,
    tool_name_by_id: HashMap<String, String>,
    md_log: Option<ConversationMdLog>,
    artifact: SessionArtifact,
    session_handle: Option<String>,
}

impl Transcript {
    fn open(agent_id: u32, artifact: SessionArtifact, repo_path: &str) -> io::Result<Self> {
        eprintln!(
            "[claude-ws] agent {agent_id}: transcript → {}",
            artifact.transcript_path().display()
        );

        let file = fs::File::create(artifact.transcript_path())?;
        Ok(Self {
            writer: BufWriter::new(file),
            verbose: transcript_verbose(),
            pending_agent: String::new(),
            tool_name_by_id: HashMap::new(),
            md_log: ConversationMdLog::open_at(
                artifact.md_path().to_path_buf(),
                agent_id,
                crate::agent::types::AgentType::ClaudeCode,
                repo_path,
                MdLogMode::StructuredAssistantOnly,
            )
            .map_err(|e| {
                eprintln!("[claude-ws] agent {agent_id}: md log disabled: {e}");
                e
            })
            .ok(),
            artifact,
            session_handle: None,
        })
    }

    fn register_session_handle(&mut self, session_handle: &str, model: Option<&str>) {
        self.session_handle = Some(session_handle.to_string());
        if let Some(ref mut md_log) = self.md_log {
            md_log.finalize_header(
                crate::agent::types::AgentType::ClaudeCode,
                self.artifact.repo_path(),
                self.artifact.started_in(),
                self.artifact.started_at(),
                session_handle,
                model,
            );
        }
        if let Err(e) = self.artifact.write_sidecar(session_handle, None) {
            eprintln!(
                "[claude-ws] failed to write session sidecar {}: {e}",
                self.artifact.sidecar_path().display()
            );
        }
    }

    /// Log a raw incoming NDJSON line from the CLI.
    fn log_incoming(&mut self, line: &str) {
        if self.verbose {
            self.write_raw("in", line);
            return;
        }
        self.log_incoming_filtered(line);
    }

    /// Log a serialized outgoing NDJSON message to the CLI.
    fn log_outgoing_raw(&mut self, json: &str) {
        if self.verbose {
            self.write_raw("out", json);
            return;
        }
        self.log_outgoing_filtered(json);
    }

    /// Log a structured metadata event (errors, closes, exits, etc.).
    fn log_meta(&mut self, event: &str, fields: serde_json::Value) {
        if self.verbose {
            let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let payload = json!({
                "event": event,
                "fields": fields,
            });
            let _ = writeln!(self.writer, r#"{{"dir":"meta","ts":"{}","msg":{}}}"#, ts, payload);
            let _ = self.writer.flush();
        }
    }

    fn write_raw(&mut self, dir: &str, line: &str) {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let _ = writeln!(
            self.writer,
            r#"{{"dir":"{}","ts":"{}","msg":{}}}"#,
            dir,
            ts,
            line.trim()
        );
        let _ = self.writer.flush();
    }

    fn write_entry(&mut self, dir: &str, msg: Value) {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let entry = json!({
            "dir": dir,
            "ts": ts,
            "msg": msg,
        });
        let _ = writeln!(self.writer, "{}", entry);
        let _ = self.writer.flush();
    }

    fn flush_agent_message(&mut self) {
        if self.pending_agent.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_agent);
        self.write_entry("in", json!({ "type": "assistant", "text": text }));
        if let Some(ref mut md) = self.md_log {
            md.append_assistant_message(&text);
        }
    }

    fn log_incoming_filtered(&mut self, line: &str) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "stream_event" => {
                if let Some(ev) = v.get("event") {
                    if ev.get("type").and_then(|v| v.as_str()) == Some("content_block_delta") {
                        if let Some(delta) = ev.get("delta") {
                            if delta.get("type").and_then(|v| v.as_str()) == Some("text_delta") {
                                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                    self.pending_agent.push_str(text);
                                }
                            }
                        }
                    }
                }
            }
            "assistant" => {
                let had_stream = !self.pending_agent.is_empty();
                let mut text_blocks = String::new();
                if let Some(content) = v.pointer("/message/content").and_then(|v| v.as_array()) {
                    for block in content {
                        match block.get("type").and_then(|v| v.as_str()) {
                            Some("text") => {
                                if !had_stream {
                                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                        text_blocks.push_str(t);
                                    }
                                }
                            }
                            Some("tool_use") => {
                                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                                let mut entry = json!({ "type": "tool_call", "tool": name });
                                if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                                    self.tool_name_by_id.insert(id.to_string(), name.to_string());
                                    if let Some(obj) = entry.as_object_mut() {
                                        obj.insert("tool_use_id".to_string(), Value::String(id.to_string()));
                                    }
                                }
                                self.write_entry("in", entry);
                            }
                            Some("tool_result") => {
                                let tool_use_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                                let tool_name = self
                                    .tool_name_by_id
                                    .get(tool_use_id)
                                    .cloned()
                                    .unwrap_or_else(|| "tool".to_string());
                                let mut entry = json!({ "type": "tool_result", "tool": tool_name });
                                if !tool_use_id.is_empty() {
                                    if let Some(obj) = entry.as_object_mut() {
                                        obj.insert("tool_use_id".to_string(), Value::String(tool_use_id.to_string()));
                                    }
                                }
                                self.write_entry("in", entry);
                            }
                            _ => {}
                        }
                    }
                }
                if !text_blocks.is_empty() {
                    self.write_entry("in", json!({ "type": "assistant", "text": text_blocks }));
                    if let Some(ref mut md) = self.md_log {
                        md.append_assistant_message(&text_blocks);
                    }
                }
                if had_stream {
                    self.flush_agent_message();
                }
            }
            "user" => {
                if let Some(content) = v.pointer("/message/content").and_then(|v| v.as_array()) {
                    for block in content {
                        if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                            let tool_use_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                            let tool_name = self
                                .tool_name_by_id
                                .get(tool_use_id)
                                .cloned()
                                .unwrap_or_else(|| "tool".to_string());
                            let mut entry = json!({ "type": "tool_result", "tool": tool_name });
                            if !tool_use_id.is_empty() {
                                if let Some(obj) = entry.as_object_mut() {
                                    obj.insert("tool_use_id".to_string(), Value::String(tool_use_id.to_string()));
                                }
                            }
                            self.write_entry("in", entry);
                        }
                    }
                }
            }
            "result" => {
                self.flush_agent_message();
            }
            _ => {}
        }
    }

    fn log_outgoing_filtered(&mut self, json_line: &str) {
        let trimmed = json_line.trim();
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return,
        };
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if msg_type != "user" {
            return;
        }
        if let Some(text) = v.pointer("/message/content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                self.write_entry("out", json!({ "type": "user", "text": text }));
                if let Some(ref mut md) = self.md_log {
                    md.append_user_prompt(text);
                }
            }
        }
    }
}

impl Drop for Transcript {
    fn drop(&mut self) {
        if let Some(ref session_handle) = self.session_handle {
            let _ = self
                .artifact
                .write_sidecar(session_handle, Some(chrono::Utc::now()));
        }
    }
}

fn append_transcript_meta(path: &Path, event: &str, fields: serde_json::Value) {
    if !transcript_verbose() {
        return;
    }
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let payload = json!({
        "event": event,
        "fields": fields,
    });
    if let Ok(file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let mut writer = BufWriter::new(file);
        let _ = writeln!(writer, r#"{{"dir":"meta","ts":"{}","msg":{}}}"#, ts, payload);
        let _ = writer.flush();
    } else {
        eprintln!(
            "[claude-ws] failed to append transcript meta event {event} to {}",
            path.display()
        );
    }
}

fn transcript_verbose() -> bool {
    std::env::var("OPENSWARM_TRANSCRIPT_VERBOSE")
        .ok()
        .as_deref()
        == Some("1")
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Spawn a Claude Code agent using the `--sdk-url` WebSocket protocol.
///
/// Returns the child process handle and a command channel for sending
/// prompts, tool approval responses, interrupts, etc.
pub(crate) fn spawn_claude_ws(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    repo_path: &str,
    prompt: Option<String>,
    resume_session_id: Option<String>,
    model: Option<String>,
) -> io::Result<(Arc<Mutex<Child>>, mpsc::Sender<ClaudeDriverCommand>, u32)> {
    let repo_name = Path::new(repo_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let artifact = SessionArtifact::create(
        crate::agent::types::AgentType::ClaudeCode,
        &repo_name,
        repo_path,
        repo_path,
        &format!("openswarm-agent-{agent_id}"),
    )?;
    let transcript_path = artifact.transcript_path().to_path_buf();
    let stderr_path = transcript_path.with_extension("stderr.log");
    let stdout_path = transcript_path.with_extension("stdout.log");

    // Bind a TCP listener on a random port
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    eprintln!("[claude-ws] agent {agent_id}: TCP listener bound on 127.0.0.1:{port}");

    // Generate auth token
    let auth_token = uuid::Uuid::new_v4().to_string();

    // Spawn the Claude CLI process
    let sdk_url = format!("ws://127.0.0.1:{port}");
    let mut args = vec![
        "--sdk-url".to_string(), sdk_url,
        "--print".to_string(),
        "--output-format".to_string(), "stream-json".to_string(),
        "--input-format".to_string(), "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    if let Some(ref m) = model {
        args.push("--model".to_string());
        args.push(m.clone());
    }
    if let Some(ref sid) = resume_session_id {
        args.push("--resume".to_string());
        args.push(sid.clone());
        eprintln!("[claude-ws] agent {agent_id}: resuming session {sid}");
    }
    args.push("-p".to_string());
    args.push(String::new());

    let child = unsafe {
        Command::new("claude")
            .args(&args)
            .current_dir(repo_path)
            .env("CLAUDE_CODE_SESSION_ACCESS_TOKEN", &auth_token)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Put the child in its own session/process group so kill_agent
            // can SIGKILL the entire tree (claude + any tool subprocesses).
            .pre_exec(|| { libc::setsid(); Ok(()) })
            .spawn()?
    };

    let child_pid = child.id();
    eprintln!("[claude-ws] agent {agent_id}: spawned claude CLI pid={child_pid}");

    let mut child = child;
    if let Some(stderr) = child.stderr.take() {
        let stderr_path = stderr_path.clone();
        eprintln!("[claude-ws] agent {agent_id}: stderr → {}", stderr_path.display());
        thread::spawn(move || {
            match fs::File::create(&stderr_path) {
                Ok(mut file) => {
                    let mut reader = BufReader::new(stderr);
                    if let Err(e) = io::copy(&mut reader, &mut file) {
                        eprintln!("[claude-ws] agent {agent_id}: stderr copy failed: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("[claude-ws] agent {agent_id}: failed to open stderr log: {e}");
                }
            }
        });
    } else {
        eprintln!("[claude-ws] agent {agent_id}: stderr capture unavailable");
    }

    if let Some(stdout) = child.stdout.take() {
        let stdout_path = stdout_path.clone();
        eprintln!("[claude-ws] agent {agent_id}: stdout → {}", stdout_path.display());
        thread::spawn(move || {
            match fs::File::create(&stdout_path) {
                Ok(mut file) => {
                    let mut reader = BufReader::new(stdout);
                    if let Err(e) = io::copy(&mut reader, &mut file) {
                        eprintln!("[claude-ws] agent {agent_id}: stdout copy failed: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("[claude-ws] agent {agent_id}: failed to open stdout log: {e}");
                }
            }
        });
    } else {
        eprintln!("[claude-ws] agent {agent_id}: stdout capture unavailable");
    }

    let child = Arc::new(Mutex::new(child));
    let (command_tx, command_rx) = mpsc::channel::<ClaudeDriverCommand>();

    // Spawn driver thread
    let state_clone = state.clone();
    let child_clone = child.clone();
    let prompt_clone = prompt;
    let repo_path_string = repo_path.to_string();
    thread::spawn(move || {
        if let Err(e) = run_driver(
            listener,
            &auth_token,
            state_clone,
            agent_id,
            child_clone,
            command_rx,
            prompt_clone,
            repo_path_string,
            artifact,
            transcript_path,
        ) {
            eprintln!("[claude-ws] agent {agent_id}: driver error: {e}");
        }
    });

    Ok((child, command_tx, child_pid))
}

/// Outer driver — cleans up (set Exited, persist, broadcast, kill child) on
/// every exit path, including early errors from the inner function.
fn run_driver(
    listener: TcpListener,
    auth_token: &str,
    state: Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    child: Arc<Mutex<Child>>,
    command_rx: mpsc::Receiver<ClaudeDriverCommand>,
    initial_prompt: Option<String>,
    repo_path: String,
    artifact: SessionArtifact,
    transcript_path: PathBuf,
) -> io::Result<()> {
    let result = run_driver_inner(
        listener,
        auth_token,
        &state,
        agent_id,
        command_rx,
        initial_prompt,
        &repo_path,
        artifact,
    );
    if let Err(ref e) = result {
        append_transcript_meta(
            &transcript_path,
            "driver_error",
            json!({ "error": e.to_string() }),
        );
    }

    // Cleanup always runs — even when inner returned Err (auth failure,
    // TCP accept error, WS error, etc.).  Without this guard those early
    // exits left the agent stuck in Running state indefinitely.
    set_status(&state, agent_id, AgentStatus::Exited);

    {
        let st = state.lock().unwrap();
        if let Some(ref store) = st.store {
            let output_tail = st
                .agents
                .get(&agent_id)
                .map(|a| {
                    const TAIL_SIZE: usize = 32 * 1024;
                    if a.output_buffer.len() > TAIL_SIZE {
                        a.output_buffer[a.output_buffer.len() - TAIL_SIZE..].to_vec()
                    } else {
                        a.output_buffer.clone()
                    }
                })
                .unwrap_or_default();
            if let Err(e) = store.mark_exited(agent_id, &output_tail) {
                eprintln!("[claude-ws] agent {agent_id}: failed to persist exit: {e}");
            }
        }
    }

    broadcast(
        &state,
        &ServerMessage::AgentStatus {
            agent_id,
            status: AgentStatus::Exited,
        },
    );

    if let Ok(mut c) = child.lock() {
        match c.try_wait() {
            Ok(Some(status)) => {
                if let Some(code) = status.code() {
                    eprintln!("[claude-ws] agent {agent_id}: claude CLI exited with code {code}");
                    append_transcript_meta(
                        &transcript_path,
                        "claude_exit",
                        json!({ "code": code }),
                    );
                } else if let Some(sig) = status.signal() {
                    eprintln!("[claude-ws] agent {agent_id}: claude CLI terminated by signal {sig}");
                    append_transcript_meta(
                        &transcript_path,
                        "claude_exit",
                        json!({ "signal": sig }),
                    );
                } else {
                    eprintln!("[claude-ws] agent {agent_id}: claude CLI exited (unknown status)");
                    append_transcript_meta(
                        &transcript_path,
                        "claude_exit",
                        json!({ "status": "unknown" }),
                    );
                }
            }
            Ok(None) => {
                eprintln!("[claude-ws] agent {agent_id}: claude CLI still running, sending kill");
                append_transcript_meta(
                    &transcript_path,
                    "claude_exit",
                    json!({ "status": "still_running" }),
                );
                let _ = c.kill();
            }
            Err(e) => {
                eprintln!("[claude-ws] agent {agent_id}: failed to query claude CLI status: {e}");
                append_transcript_meta(
                    &transcript_path,
                    "claude_exit",
                    json!({ "status": "unknown", "error": e.to_string() }),
                );
                let _ = c.kill();
            }
        }
    }

    eprintln!("[claude-ws] agent {agent_id}: driver exiting");
    result
}

/// Inner driver loop — accepts a WS connection, then polls for messages
/// and commands in a single thread.  Returns Err on any fatal error;
/// the outer wrapper handles all cleanup.
fn run_driver_inner(
    listener: TcpListener,
    auth_token: &str,
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    command_rx: mpsc::Receiver<ClaudeDriverCommand>,
    initial_prompt: Option<String>,
    repo_path: &str,
    artifact: SessionArtifact,
) -> io::Result<()> {
    // Open transcript log
    let mut transcript = match Transcript::open(agent_id, artifact, repo_path) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("[claude-ws] agent {agent_id}: failed to open transcript: {e}");
            None
        }
    };

    // Wait for Claude CLI to connect (blocking)
    listener.set_nonblocking(false)?;
    let tcp_stream = {
        listener
            .incoming()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "no connection"))?
    }?;

    eprintln!("[claude-ws] agent {agent_id}: TCP connection from Claude CLI");

    // Validate Bearer token during WS handshake
    let expected_token = format!("Bearer {auth_token}");
    let auth_ok = Arc::new(Mutex::new(false));
    let auth_ok_clone = auth_ok.clone();
    let expected_clone = expected_token.clone();

    let mut ws = tungstenite::accept_hdr(tcp_stream, move |req: &Request, resp: Response| {
        if let Some(auth_header) = req.headers().get("authorization") {
            if auth_header.to_str().unwrap_or("") == expected_clone {
                *auth_ok_clone.lock().unwrap() = true;
            }
        }
        Ok(resp)
    })
    .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;

    if !*auth_ok.lock().unwrap() {
        eprintln!("[claude-ws] agent {agent_id}: auth token mismatch, closing");
        if let Some(ref mut t) = transcript {
            t.log_meta(
                "auth_mismatch",
                json!({ "reason": "missing or invalid authorization header" }),
            );
        }
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "auth token mismatch",
        ));
    }

    eprintln!("[claude-ws] agent {agent_id}: WS handshake + auth OK");
    if let Some(ref mut t) = transcript {
        t.log_meta("ws_handshake_ok", json!({}));
    }

    // Set a short read timeout so the poll loop can interleave WS reads
    // with processing supervisor commands.  This is the key fix: without it
    // ws.read() blocks the thread indefinitely, preventing outgoing sends.
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(50)))?;

    // The CLI waits indefinitely for the server to send the first `user`
    // message (per protocol §2).  Send it now with session_id="" — the CLI
    // will respond with `system/init` containing the real session_id.
    let prompt_text = initial_prompt
        .as_deref()
        .filter(|p| !p.is_empty())
        .unwrap_or("What can you help me with?");

    let user_msg = UserMessage::new(prompt_text.to_string(), String::new());
    let prompt_ansi = ansi_render::render_user_prompt(prompt_text);
    emit_output(&state, agent_id, &prompt_ansi);
    ws_send(&mut ws, agent_id, &user_msg, &mut transcript);
    eprintln!("[claude-ws] agent {agent_id}: sent initial user message");

    let mut session_id = String::new();
    let mut pending_approvals: HashMap<String, PendingToolApproval> = HashMap::new();

    // Single-threaded poll loop
    loop {
        // 1. Try reading from WebSocket (returns quickly on timeout)
        match ws.read() {
            Ok(Message::Text(text)) => {
                // NDJSON can have multiple lines per WS message
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        // Log raw incoming before parsing
                        if let Some(ref mut t) = transcript {
                            t.log_incoming(line);
                        }
                        match claude_proto::parse_incoming(line) {
                            Ok(msg) => {
                                if let Some(pa) = handle_incoming(
                                    &state,
                                    agent_id,
                                    msg,
                                    &mut session_id,
                                    &mut transcript,
                                ) {
                                    pending_approvals.insert(pa.request_id.clone(), pa);
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "[claude-ws] agent {agent_id}: parse error: {e}"
                                );
                            }
                        }
                    }
                }
            }
            Ok(Message::Close(frame)) => {
                eprintln!("[claude-ws] agent {agent_id}: WS closed by peer");
                if let Some(ref mut t) = transcript {
                    if let Some(frame) = frame {
                        let code: u16 = frame.code.into();
                        let reason = frame.reason.to_string();
                        t.log_meta("ws_close", json!({ "code": code, "reason": reason }));
                    } else {
                        t.log_meta("ws_close", json!({ "code": null, "reason": null }));
                    }
                }
                break;
            }
            Ok(_) => {} // Ping/Pong/Binary — ignore
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                // Normal read timeout — fall through to process commands
            }
            Err(e) => {
                eprintln!("[claude-ws] agent {agent_id}: WS error: {e}");
                if let Some(ref mut t) = transcript {
                    t.log_meta("ws_error", json!({ "error": e.to_string() }));
                }
                break;
            }
        }

        // 2. Drain any pending commands from the supervisor
        let mut should_exit = false;
        loop {
            match command_rx.try_recv() {
                Ok(ClaudeDriverCommand::Shutdown) => {
                    handle_command(
                        &state, agent_id,
                        ClaudeDriverCommand::Shutdown,
                        &mut ws, &session_id,
                        &mut transcript,
                    );
                    should_exit = true;
                    break;
                }
                // Handle tool approval responses inline (needs access to pending approvals)
                Ok(ClaudeDriverCommand::ToolApprovalResponse {
                    request_id, approved, updated_input,
                }) => {
                    if let Some(pending) = pending_approvals.remove(&request_id) {
                        if approved {
                            let input = updated_input.unwrap_or(pending.tool_input.clone());
                            let response = ControlResponse::allow(request_id, input);
                            let ansi = ansi_render::render_approval_granted(&pending.tool_name);
                            emit_output(&state, agent_id, &ansi);
                            ws_send(&mut ws, agent_id, &response, &mut transcript);
                        } else {
                            let response = ControlResponse::deny(request_id, "Denied by user".to_string());
                            let ansi = ansi_render::render_approval_denied(&pending.tool_name, "Denied by user");
                            emit_output(&state, agent_id, &ansi);
                            ws_send(&mut ws, agent_id, &response, &mut transcript);
                        }
                    }
                    // If no matching pending, ignore (stale response)
                }
                Ok(cmd) => {
                    handle_command(
                        &state, agent_id, cmd, &mut ws, &session_id,
                        &mut transcript,
                    );
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    eprintln!("[claude-ws] agent {agent_id}: command channel closed");
                    should_exit = true;
                    break;
                }
            }
        }
        if should_exit {
            break;
        }

    }

    Ok(())
}

/// Handle an incoming NDJSON message from Claude CLI.
fn handle_incoming(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    msg: IncomingMessage,
    session_id: &mut String,
    transcript: &mut Option<Transcript>,
) -> Option<PendingToolApproval> {
    match msg {
        IncomingMessage::SystemInit(init) => {
            eprintln!(
                "[claude-ws] agent {agent_id}: init session={} model={} tools={}",
                init.session_id,
                init.model,
                init.tools.len()
            );

            *session_id = init.session_id.clone();
            if let Some(ref mut transcript) = transcript {
                transcript.register_session_handle(&init.session_id, Some(&init.model));
            }

            // Persist session_id into AgentRuntime + SQLite
            {
                let mut st = state.lock().unwrap();
                if let Some(agent) = st.agents.get_mut(&agent_id) {
                    agent.session_id = Some(init.session_id.clone());
                }
                if let Some(ref store) = st.store {
                    if let Err(e) = store.update_session_id(agent_id, &init.session_id) {
                        eprintln!("[claude-ws] agent {agent_id}: failed to persist session_id: {e}");
                    }
                }
            }

            // Render ANSI init header
            let ansi = ansi_render::render_init(&init);
            emit_output(state, agent_id, &ansi);

            // Emit structured event for dashboard
            emit_event(state, agent_id, AgentEventType::SessionInit {
                model: init.model.clone(),
                session_id: init.session_id.clone(),
            });

            // Note: the initial user message was already sent right after
            // the WS handshake (the CLI requires it before sending init).
        }

        IncomingMessage::StreamEvent(se) => {
            // Render streaming text/thinking deltas for ticker effect
            if let Some(ansi) = ansi_render::render_stream_delta(&se) {
                emit_output(state, agent_id, &ansi);
            }
        }

        IncomingMessage::Assistant(assistant) => {
            // Render non-text blocks (tool_use, tool_result, thinking)
            // Text was already streamed via stream_event deltas
            let ansi = ansi_render::render_assistant_blocks(&assistant);
            if !ansi.is_empty() {
                emit_output(state, agent_id, &ansi);
            }

            // Emit structured events for tool_use blocks
            for block in &assistant.message.content {
                match block {
                    claude_proto::ContentBlock::ToolUse { name, .. } => {
                        emit_event(
                            state,
                            agent_id,
                            AgentEventType::ToolStart {
                                tool_name: name.clone(),
                            },
                        );
                    }
                    claude_proto::ContentBlock::ToolResult {
                        is_error, ..
                    } => {
                        emit_event(
                            state,
                            agent_id,
                            AgentEventType::ToolEnd {
                                tool_name: String::new(),
                                success: !is_error,
                                duration_ms: 0,
                            },
                        );
                    }
                    claude_proto::ContentBlock::Thinking { .. } => {
                        emit_event(state, agent_id, AgentEventType::Thinking);
                    }
                    _ => {}
                }
            }

            // Emit token usage if available
            if let Some(usage) = &assistant.message.usage {
                emit_event(
                    state,
                    agent_id,
                    AgentEventType::TokenUsage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                    },
                );
            }
        }

        IncomingMessage::Result(result) => {
            let ansi = ansi_render::render_result(&result);
            emit_output(state, agent_id, &ansi);

            emit_event(
                state,
                agent_id,
                AgentEventType::CostUpdate {
                    total_dollars: result.total_cost_usd,
                },
            );
            emit_event(
                state,
                agent_id,
                AgentEventType::QueryComplete {
                    num_turns: result.num_turns,
                    duration_ms: result.duration_ms,
                    is_error: result.is_error,
                },
            );

            eprintln!(
                "[claude-ws] agent {agent_id}: query complete cost=${:.4} turns={} {}",
                result.total_cost_usd, result.num_turns, result.subtype
            );
        }

        IncomingMessage::ToolProgress(progress) => {
            let ansi = ansi_render::render_tool_progress(&progress);
            emit_output(state, agent_id, &ansi);
        }

        IncomingMessage::ControlRequest(cr) => {
            let subtype = cr
                .request
                .get("subtype")
                .and_then(|s| s.as_str())
                .unwrap_or("");

            if subtype == "can_use_tool" {
                return handle_can_use_tool(state, agent_id, &cr);
            } else {
                eprintln!(
                    "[claude-ws] agent {agent_id}: unhandled control_request subtype: {subtype}"
                );
            }
        }

        IncomingMessage::SystemStatus(status) => {
            let ansi = ansi_render::render_status(status.status.as_deref());
            emit_output(state, agent_id, &ansi);
        }

        IncomingMessage::KeepAlive => {
            // Silently consumed
        }

        IncomingMessage::Unknown { msg_type } => {
            eprintln!("[claude-ws] agent {agent_id}: unknown message type: {msg_type}");
        }
    }
    None
}

/// Pending tool approval awaiting an explicit user response.
struct PendingToolApproval {
    request_id: String,
    tool_input: serde_json::Value,
    tool_name: String,
}

/// Handle a `can_use_tool` permission request.
///
/// Broadcasts the request to UI clients and returns a PendingToolApproval.
fn handle_can_use_tool(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    cr: &claude_proto::ControlRequest,
) -> Option<PendingToolApproval> {
    let payload: CanUseToolRequest = match serde_json::from_value(cr.request.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[claude-ws] agent {agent_id}: failed to parse can_use_tool: {e}");
            return None;
        }
    };

    // Render approval request in ANSI
    let ansi = ansi_render::render_approval_request(&payload);
    emit_output(state, agent_id, &ansi);

    // Broadcast structured events to all UI clients
    broadcast(
        state,
        &ServerMessage::AgentEvent {
            agent_id,
            timestamp: now_millis(),
            event: AgentEventType::WaitingForInput,
        },
    );
    broadcast(
        state,
        &ServerMessage::ToolApprovalRequest {
            agent_id,
            request_id: cr.request_id.clone(),
            tool_name: payload.tool_name.clone(),
            tool_input: payload.input.clone(),
            description: payload.description.clone(),
        },
    );

    eprintln!(
        "[claude-ws] agent {agent_id}: tool approval pending {} (req_id={})",
        payload.tool_name, cr.request_id
    );

    Some(PendingToolApproval {
        request_id: cr.request_id.clone(),
        tool_input: payload.input,
        tool_name: payload.tool_name,
    })
}

/// Handle a command from the supervisor (prompt, approval response, interrupt).
fn handle_command(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    cmd: ClaudeDriverCommand,
    ws: &mut tungstenite::WebSocket<std::net::TcpStream>,
    session_id: &str,
    transcript: &mut Option<Transcript>,
) {
    match cmd {
        ClaudeDriverCommand::SendPrompt { prompt } => {
            let user_msg = UserMessage::new(prompt.clone(), session_id.to_string());
            let ansi = ansi_render::render_user_prompt(&prompt);
            emit_output(state, agent_id, &ansi);
            ws_send(ws, agent_id, &user_msg, transcript);
        }

        ClaudeDriverCommand::ToolApprovalResponse { .. } => {
            // Handled inline in the poll loop (needs access to pending_approval state)
            eprintln!("[claude-ws] agent {agent_id}: unexpected ToolApprovalResponse in handle_command");
        }

        ClaudeDriverCommand::Interrupt => {
            let request_id = uuid::Uuid::new_v4().to_string();
            let request = OutgoingControlRequest::interrupt(request_id);
            ws_send(ws, agent_id, &request, transcript);
            eprintln!("[claude-ws] agent {agent_id}: sent interrupt");
        }

        ClaudeDriverCommand::Shutdown => {
            eprintln!("[claude-ws] agent {agent_id}: shutdown requested");
            let _ = ws.close(None);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Send a serialized NDJSON message over the WebSocket and log to transcript.
fn ws_send<T: serde::Serialize>(
    ws: &mut tungstenite::WebSocket<std::net::TcpStream>,
    agent_id: u32,
    msg: &T,
    transcript: &mut Option<Transcript>,
) {
    match to_ndjson(msg) {
        Ok(ndjson) => {
            if let Some(ref mut t) = transcript {
                t.log_outgoing_raw(&ndjson);
            }
            if let Err(e) = ws.send(Message::Text(ndjson)) {
                eprintln!("[claude-ws] agent {agent_id}: WS send error: {e}");
            }
        }
        Err(e) => {
            eprintln!("[claude-ws] agent {agent_id}: serialize error: {e}");
        }
    }
}

/// Emit ANSI-formatted output bytes to the agent's buffer and broadcast.
fn emit_output(state: &Arc<Mutex<SupervisorState>>, agent_id: u32, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    append_output(state, agent_id, bytes);

    let paused = state
        .lock()
        .map(|s| {
            s.agents
                .get(&agent_id)
                .map_or(false, |a| a.output_paused)
        })
        .unwrap_or(false);

    if !paused {
        broadcast(
            state,
            &ServerMessage::AgentOutput {
                agent_id,
                bytes: bytes.to_vec(),
            },
        );
    }
}

/// Emit a structured AgentEvent.
fn emit_event(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    event: AgentEventType,
) {
    broadcast(
        state,
        &ServerMessage::AgentEvent {
            agent_id,
            timestamp: now_millis(),
            event,
        },
    );
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
