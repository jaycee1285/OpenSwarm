//! Codex app-server driver.
//!
//! Spawns `codex app-server` as a subprocess, communicates via line-delimited
//! JSON-RPC over stdin/stdout.  The driver runs on a dedicated thread with a
//! blocking read loop (one line at a time) interleaved with command channel
//! polling.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::os::unix::process::CommandExt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chrono::TimeZone;
use serde_json::{json, Value};

use super::codex_proto::{self, IncomingMessage};
use super::codex_render;
use super::md_log::{ConversationMdLog, MdLogMode};
use super::session_artifact::SessionArtifact;
use super::server::{
    append_output, broadcast, emit_event, set_status, update_codex_rate_limits, SupervisorState,
};
use crate::agent::status::AgentStatus;
use crate::ipc::proto::{AgentEventType, ServerMessage};

struct Transcript {
    writer: BufWriter<fs::File>,
    verbose: bool,
    pending_agent: String,
    md_log: Option<ConversationMdLog>,
    artifact: SessionArtifact,
    session_handle: Option<String>,
}

impl Transcript {
    fn open(agent_id: u32, repo_name: &str, repo_path: &str) -> io::Result<Self> {
        let artifact = SessionArtifact::create(
            crate::agent::types::AgentType::Codex,
            repo_name,
            repo_path,
            repo_path,
            &format!("openswarm-agent-{agent_id}"),
        )?;
        eprintln!(
            "[codex] agent {agent_id}: transcript → {}",
            artifact.transcript_path().display()
        );

        let file = fs::File::create(artifact.transcript_path())?;
        Ok(Self {
            writer: BufWriter::new(file),
            verbose: transcript_verbose(),
            pending_agent: String::new(),
            md_log: ConversationMdLog::open_at(
                artifact.md_path().to_path_buf(),
                agent_id,
                crate::agent::types::AgentType::Codex,
                repo_path,
                MdLogMode::StructuredAssistantOnly,
            )
            .map_err(|e| {
                eprintln!("[codex] agent {agent_id}: md log disabled: {e}");
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
                crate::agent::types::AgentType::Codex,
                self.artifact.repo_path(),
                self.artifact.started_in(),
                self.artifact.started_at(),
                session_handle,
                model,
            );
        }
        if let Err(e) = self.artifact.write_sidecar(session_handle, None) {
            eprintln!(
                "[codex] failed to write session sidecar {}: {e}",
                self.artifact.sidecar_path().display()
            );
        }
    }

    fn log_incoming(&mut self, line: &str) {
        if self.verbose {
            self.write_raw("in", line);
            return;
        }
        self.log_incoming_filtered(line);
    }

    fn log_outgoing(&mut self, line: &str) {
        if self.verbose {
            self.write_raw("out", line);
            return;
        }
        self.log_outgoing_filtered(line);
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
        self.write_entry("in", json!({"type": "assistant", "text": text}));
        if let Some(ref mut md) = self.md_log {
            md.append_assistant_message(&text);
        }
    }

    fn log_incoming_filtered(&mut self, line: &str) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let method = match v.get("method").and_then(|m| m.as_str()) {
            Some(m) => m,
            None => return,
        };
        match method {
            "item/agentMessage/delta" => {
                if let Some(delta) = v.pointer("/params/delta").and_then(|v| v.as_str()) {
                    self.pending_agent.push_str(delta);
                }
            }
            "item/started" => {
                if let Some(item) = v.pointer("/params/item") {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if item_type != "agentMessage" {
                        let tool_name = item
                            .get("tool")
                            .and_then(|t| t.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(item_type);
                        let mut entry = json!({"type": "tool_call", "tool": tool_name});
                        if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                            if let Some(obj) = entry.as_object_mut() {
                                obj.insert("item_id".to_string(), Value::String(id.to_string()));
                            }
                        }
                        self.write_entry("in", entry);
                    }
                }
            }
            "item/completed" => {
                if let Some(item) = v.pointer("/params/item") {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if item_type == "agentMessage" {
                        self.flush_agent_message();
                    } else {
                        let tool_name = item
                            .get("tool")
                            .and_then(|t| t.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(item_type);
                        let mut entry = json!({"type": "tool_result", "tool": tool_name});
                        if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                            if let Some(obj) = entry.as_object_mut() {
                                obj.insert("item_id".to_string(), Value::String(id.to_string()));
                            }
                        }
                        self.write_entry("in", entry);
                    }
                }
            }
            "turn/completed" => {
                self.flush_agent_message();
            }
            _ => {}
        }
    }

    fn log_outgoing_filtered(&mut self, line: &str) {
        let trimmed = line.trim();
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return,
        };
        let method = match v.get("method").and_then(|m| m.as_str()) {
            Some(m) => m,
            None => return,
        };
        if method != "turn/start" {
            return;
        }
        let mut text = String::new();
        if let Some(inputs) = v.pointer("/params/input").and_then(|v| v.as_array()) {
            for item in inputs {
                if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
            }
        }
        if !text.is_empty() {
            self.write_entry("out", json!({"type": "user", "text": text}));
            if let Some(ref mut md) = self.md_log {
                md.append_user_prompt(&text);
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

fn transcript_verbose() -> bool {
    std::env::var("OPENSWARM_TRANSCRIPT_VERBOSE")
        .ok()
        .as_deref()
        == Some("1")
}

// ---------------------------------------------------------------------------
// Driver command channel
// ---------------------------------------------------------------------------

pub(crate) enum CodexDriverCommand {
    SendPrompt { prompt: String },
    ToolApprovalResponse {
        request_id: Value,
        approved: bool,
    },
    Interrupt,
    RefreshUsage,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingCodexRequestKind {
    StandardApproval,
    ReviewDecision,
    UserInput,
    DynamicToolCall,
}

/// Pending request awaiting an explicit user response.
struct PendingToolApproval {
    request_id_key: String,
    request_id: Value,
    tool_name: String,
    kind: PendingCodexRequestKind,
}

fn is_executable_file(path: &Path) -> bool {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
}

fn resolve_binary_from_path(binary: &str) -> Option<PathBuf> {
    if binary.contains('/') {
        let path = PathBuf::from(binary);
        if is_executable_file(&path) {
            return Some(path);
        }
        return None;
    }
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn resolve_codex_binary() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("OPENSWARM_CODEX_BIN") {
        let path = PathBuf::from(configured);
        if is_executable_file(&path) {
            return Some(path);
        }
    }
    resolve_binary_from_path("codex")
}

fn build_codex_command() -> Command {
    if let Some(codex_bin) = resolve_codex_binary() {
        if let Some(parent) = codex_bin.parent() {
            let sibling_node = parent.join("node");
            if is_executable_file(&sibling_node) {
                let mut cmd = Command::new(sibling_node);
                cmd.arg(codex_bin);
                return cmd;
            }
        }
        return Command::new(codex_bin);
    }
    Command::new("codex")
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawn `codex app-server` and return the child process + command channel.
pub(crate) fn spawn_codex(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    repo_path: &str,
    prompt: Option<String>,
    resume_thread_id: Option<String>,
    model: Option<String>,
) -> io::Result<(Arc<Mutex<Child>>, mpsc::Sender<CodexDriverCommand>, u32)> {
    let mut cmd = build_codex_command();
    cmd.arg("app-server");
    cmd.arg("-c").arg("tools.webSearch=true");

    // Keep Codex startup deterministic and isolated from Claude env hints.
    cmd.env_remove("CLAUDECODE");
    if env::var_os("CODEX_HOME").is_none() {
        if let Some(home) = env::var_os("HOME") {
            let mut codex_home = PathBuf::from(home);
            codex_home.push(".codex");
            cmd.env("CODEX_HOME", codex_home);
        }
    }
    let mut child = unsafe {
        cmd.current_dir(repo_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own session/process group for clean tree kill.
            .pre_exec(|| { libc::setsid(); Ok(()) })
            .spawn()?
    };

    let child_pid = child.id();
    eprintln!("[codex] agent {agent_id}: spawned codex app-server pid={child_pid}");

    let stdin = child.stdin.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "failed to capture stdin")
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "failed to capture stdout")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "failed to capture stderr")
    })?;

    let child = Arc::new(Mutex::new(child));
    let (command_tx, command_rx) = mpsc::channel::<CodexDriverCommand>();

    let state_clone = state.clone();
    let child_clone = child.clone();
    let repo_path = repo_path.to_string();

    thread::spawn(move || {
        if let Err(e) = run_driver(
            stdin,
            stdout,
            stderr,
            state_clone,
            agent_id,
            child_clone,
            command_rx,
            prompt,
            &repo_path,
            resume_thread_id,
            model,
        ) {
            eprintln!("[codex] agent {agent_id}: driver error: {e}");
        }
    });

    Ok((child, command_tx, child_pid))
}

// ---------------------------------------------------------------------------
// Driver loop
// ---------------------------------------------------------------------------

/// Outer driver — guarantees cleanup runs on every exit path.
fn run_driver(
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    state: Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    child: Arc<Mutex<Child>>,
    command_rx: mpsc::Receiver<CodexDriverCommand>,
    initial_prompt: Option<String>,
    repo_path: &str,
    resume_thread_id: Option<String>,
    model: Option<String>,
) -> io::Result<()> {
    let result = run_driver_inner(
        stdin, stdout, stderr, &state, agent_id, &child, command_rx, initial_prompt,
        repo_path, resume_thread_id, model,
    );

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
                eprintln!("[codex] agent {agent_id}: failed to persist exit: {e}");
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
        let _ = c.kill();
    }

    eprintln!("[codex] agent {agent_id}: driver exiting");
    result
}

/// Inner driver loop.  Returns Err on fatal errors; outer wrapper cleans up.
fn is_unsupported_model_error(err: &Value) -> bool {
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("model")
        && (lower.contains("not supported")
            || lower.contains("unsupported")
            || lower.contains("unknown model"))
}

 fn run_driver_inner(
    mut stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    child: &Arc<Mutex<Child>>,
    command_rx: mpsc::Receiver<CodexDriverCommand>,
    initial_prompt: Option<String>,
    repo_path: &str,
    resume_thread_id: Option<String>,
    model: Option<String>,
) -> io::Result<()> {
    // Open transcript log
    let repo_name = Path::new(repo_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let mut transcript = match Transcript::open(agent_id, &repo_name, repo_path) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("[codex] agent {agent_id}: failed to open transcript: {e}");
            None
        }
    };

    // Spawn stderr reader (just log it)
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(line) => eprintln!("[codex-stderr] {line}"),
                Err(_) => break,
            }
        }
    });

    // --- Initialize handshake ---
    let init_req = codex_proto::initialize_request("openswarm", env!("CARGO_PKG_VERSION"));
    let init_id = init_req.id;
    send_line(&mut stdin, &codex_proto::to_ndjson_request(&init_req), &mut transcript)?;
    eprintln!("[codex] agent {agent_id}: sent initialize request (id={init_id})");

    // Read lines until we get the initialize response
    let mut reader = BufReader::new(stdout);
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "codex stdout closed during init",
            ));
        }
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(ref mut t) = transcript {
            t.log_incoming(trimmed);
        }
        match codex_proto::parse_incoming(trimmed) {
            Ok(IncomingMessage::Response { id, error, .. }) => {
                if id == init_id {
                    if let Some(err) = error {
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("initialize failed: {err}"),
                        ));
                    }
                    eprintln!("[codex] agent {agent_id}: initialize response OK");
                    break;
                }
            }
            Ok(IncomingMessage::Notification { method, .. }) => {
                eprintln!("[codex] agent {agent_id}: pre-init notification: {method}");
            }
            Err(e) => {
                eprintln!("[codex] agent {agent_id}: pre-init parse error: {e}");
            }
        }
    }

    // Send initialized notification
    send_line(
        &mut stdin,
        &codex_proto::to_ndjson_notification(&codex_proto::initialized_notification()),
        &mut transcript,
    )?;
    eprintln!("[codex] agent {agent_id}: sent initialized notification");

    // Kick off initial Codex account usage limits read (best-effort).
    let mut pending_rate_limit_reads: HashSet<u64> = HashSet::new();
    let rl_req = codex_proto::account_rate_limits_read_request();
    pending_rate_limit_reads.insert(rl_req.id);
    let _ = send_line(
        &mut stdin,
        &codex_proto::to_ndjson_request(&rl_req),
        &mut transcript,
    );

    // --- Start or resume thread ---
    let thread_label = if resume_thread_id.is_some() {
        "thread/resume"
    } else {
        "thread/start"
    };
    let mut requested_model = model.clone();
    let mut requested_model_label = requested_model.clone();
    let mut retried_without_model = false;
    let thread_req = if let Some(ref tid) = resume_thread_id {
        codex_proto::thread_resume_request(tid, repo_path, requested_model.as_deref())
    } else {
        codex_proto::thread_start_request(repo_path, requested_model.as_deref())
    };
    let mut thread_req_id = thread_req.id;
    send_line(&mut stdin, &codex_proto::to_ndjson_request(&thread_req), &mut transcript)?;
    eprintln!("[codex] agent {agent_id}: sent {thread_label} (id={thread_req_id}) model={:?}", requested_model_label);

    // Wait for thread/start response to get thread_id and model
    let mut thread_id = String::new();
    let mut reported_model = String::from("codex");

    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("codex stdout closed during {thread_label}"),
            ));
        }
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(ref mut t) = transcript {
            t.log_incoming(trimmed);
        }
        match codex_proto::parse_incoming(trimmed) {
            Ok(IncomingMessage::Response { id, result, error }) => {
                if pending_rate_limit_reads.remove(&id) {
                    if let Some(ref r) = result {
                        update_codex_from_payload(&state, r);
                    }
                    continue;
                }
                if id == thread_req_id {
                    if let Some(err) = error {
                        if requested_model.is_some()
                            && !retried_without_model
                            && is_unsupported_model_error(&err)
                        {
                            eprintln!(
                                "[codex] agent {agent_id}: requested model {:?} rejected by app-server, retrying without explicit model: {err}",
                                requested_model_label
                            );
                            requested_model = None;
                            requested_model_label = None;
                            retried_without_model = true;
                            let retry_req = if let Some(ref tid) = resume_thread_id {
                                codex_proto::thread_resume_request(tid, repo_path, None)
                            } else {
                                codex_proto::thread_start_request(repo_path, None)
                            };
                            thread_req_id = retry_req.id;
                            send_line(
                                &mut stdin,
                                &codex_proto::to_ndjson_request(&retry_req),
                                &mut transcript,
                            )?;
                            eprintln!(
                                "[codex] agent {agent_id}: retried {thread_label} without explicit model (id={thread_req_id})"
                            );
                            continue;
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("{thread_label} failed: {err}"),
                        ));
                    }
                    // Extract thread ID and model from result
                    if let Some(ref r) = result {
                        thread_id = r
                            .get("thread")
                            .or_else(|| r.get("threadId"))
                            .and_then(|t| {
                                t.get("id")
                                    .and_then(|v| v.as_str())
                                    .or_else(|| t.as_str())
                            })
                            .unwrap_or("")
                            .to_string();
                        // thread/start response includes "model" at top level
                        if let Some(m) = r.get("model").and_then(|v| v.as_str()) {
                            reported_model = m.to_string();
                        }
                    }
                    eprintln!("[codex] agent {agent_id}: thread started: {thread_id} model={reported_model}");
                    break;
                }
            }
            Ok(IncomingMessage::Notification { method, params, .. }) => {
                // Handle early notifications (thread/started, codex/connected, etc.)
                handle_notification(&state, agent_id, &method, &params);
            }
            Err(e) => {
                eprintln!("[codex] agent {agent_id}: {thread_label} parse error: {e}");
            }
        }
    }

    // Store thread ID (Codex-specific; distinct from Claude's session_id)
    {
        let mut st = state.lock().unwrap();
        if let Some(agent) = st.agents.get_mut(&agent_id) {
            agent.thread_id = Some(thread_id.clone());
        }
        if let Some(ref store) = st.store {
            if let Err(e) = store.update_thread_id(agent_id, &thread_id) {
                eprintln!("[codex] agent {agent_id}: failed to persist thread_id: {e}");
            }
        }
    }
    if let Some(ref mut t) = transcript {
        t.register_session_handle(&thread_id, Some(&reported_model));
    }

    emit_output(&state, agent_id, &codex_render::render_session_header(&thread_id));
    emit_event(
        &state,
        agent_id,
        AgentEventType::SessionInit {
            model: reported_model,
            session_id: thread_id.clone(),
        },
    );

    // --- Send initial prompt if provided ---
    let mut current_turn_id = String::new();
    if let Some(ref prompt) = initial_prompt {
        if !prompt.is_empty() {
            let turn_req = codex_proto::turn_start_request(&thread_id, prompt, repo_path);
            send_line(&mut stdin, &codex_proto::to_ndjson_request(&turn_req), &mut transcript)?;
            emit_output(&state, agent_id, &codex_render::render_user_prompt(prompt));
            eprintln!("[codex] agent {agent_id}: sent initial turn/start");
        }
    }

    // --- Main poll loop ---
    // ChildStdout pipes don't support read timeouts, so we move blocking
    // read_line to a dedicated thread and receive lines through a channel.
    // This lets the main loop service command_rx (SendPrompt, etc.) even
    // while codex is idle and producing no output.
    let (line_tx, line_rx) = mpsc::channel::<Option<String>>();
    thread::spawn(move || {
        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) => {
                    let _ = line_tx.send(None); // EOF
                    break;
                }
                Ok(_) => {
                    let _ = line_tx.send(Some(buf.clone()));
                }
                Err(_) => {
                    let _ = line_tx.send(None); // Error → treat as EOF
                    break;
                }
            }
        }
    });

    let mut pending_approvals: HashMap<String, PendingToolApproval> = HashMap::new();

    loop {
        // Poll for stdout lines with a short timeout so we can service commands
        match line_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(None) => {
                // EOF from stdout reader
                eprintln!("[codex] agent {agent_id}: stdout EOF");
                break;
            }
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    if let Some(ref mut t) = transcript {
                        t.log_incoming(trimmed);
                    }
                    match codex_proto::parse_incoming(trimmed) {
                        Ok(IncomingMessage::Notification { id, method, params }) => {
                            // Some Codex server requests arrive as method+id and require a JSON-RPC response.
                            if let Some(req_id) = id {
                                if let Some(pending) = handle_request_requiring_response(
                                    &state,
                                    agent_id,
                                    req_id,
                                    &method,
                                    &params,
                                ) {
                                    pending_approvals.insert(pending.request_id_key.clone(), pending);
                                    continue;
                                }
                            }

                            handle_notification(&state, agent_id, &method, &params);

                            // Track turn ID from turn/started
                            if method == "turn/started" {
                                if let Some(tid) = codex_proto::extract_turn_id(&params) {
                                    current_turn_id = tid;
                                }
                            }
                        }
                        Ok(IncomingMessage::Response { id, result, error }) => {
                            if pending_rate_limit_reads.remove(&id) {
                                if let Some(ref r) = result {
                                    update_codex_from_payload(&state, r);
                                }
                                continue;
                            }
                            if let Some(err) = error {
                                eprintln!("[codex] agent {agent_id}: response error (id={id}): {err}");
                            } else {
                                eprintln!("[codex] agent {agent_id}: response ok (id={id})");
                                let _ = result; // consume
                            }
                        }
                        Err(e) => {
                            eprintln!("[codex] agent {agent_id}: parse error: {e}");
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // No output — fall through to drain commands
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("[codex] agent {agent_id}: line reader disconnected");
                break;
            }
        }

        // Drain commands
        let mut should_exit = false;
        loop {
            match command_rx.try_recv() {
                Ok(CodexDriverCommand::SendPrompt { prompt }) => {
                    let turn_req =
                        codex_proto::turn_start_request(&thread_id, &prompt, repo_path);
                    emit_output(&state, agent_id, &codex_render::render_user_prompt(&prompt));
                    let _ = send_line(&mut stdin, &codex_proto::to_ndjson_request(&turn_req), &mut transcript);
                }
                // Handle request responses inline (needs access to pending request map)
                Ok(CodexDriverCommand::ToolApprovalResponse {
                    request_id,
                    approved,
                }) => {
                    let request_key = request_id.to_string();
                    if let Some(pending) = pending_approvals.remove(&request_key) {
                        let resp = match pending.kind {
                            PendingCodexRequestKind::StandardApproval => {
                                codex_proto::approval_response(pending.request_id.clone(), approved)
                            }
                            PendingCodexRequestKind::ReviewDecision => codex_proto::JsonRpcResponseOut {
                                id: pending.request_id.clone(),
                                result: json!({
                                    "decision": if approved { "approved" } else { "denied" }
                                }),
                            },
                            PendingCodexRequestKind::UserInput => codex_proto::JsonRpcResponseOut {
                                id: pending.request_id.clone(),
                                result: json!({ "answers": {} }),
                            },
                            PendingCodexRequestKind::DynamicToolCall => codex_proto::JsonRpcResponseOut {
                                id: pending.request_id.clone(),
                                result: json!({
                                    "contentItems": [{
                                        "type": "inputText",
                                        "text": if approved {
                                            format!("Dynamic tool '{}' approved by user", pending.tool_name)
                                        } else {
                                            format!("Dynamic tool '{}' denied by user", pending.tool_name)
                                        }
                                    }],
                                    "success": approved,
                                }),
                            },
                        };
                        let _ = send_line(&mut stdin, &codex_proto::to_ndjson_response(&resp), &mut transcript);
                        if approved {
                            emit_output(
                                &state,
                                agent_id,
                                &codex_render::render_approval_granted(&pending.tool_name),
                            );
                        } else {
                            emit_output(
                                &state,
                                agent_id,
                                &codex_render::render_approval_denied(&pending.tool_name),
                            );
                        }
                    }
                    // If no matching pending, ignore (stale response)
                }
                Ok(CodexDriverCommand::Interrupt) => {
                    if !current_turn_id.is_empty() {
                        let req = codex_proto::turn_interrupt_request(
                            &thread_id,
                            &current_turn_id,
                        );
                        let _ = send_line(&mut stdin, &codex_proto::to_ndjson_request(&req), &mut transcript);
                        eprintln!("[codex] agent {agent_id}: sent turn/interrupt");
                    }
                }
                Ok(CodexDriverCommand::RefreshUsage) => {
                    let req = codex_proto::account_rate_limits_read_request();
                    pending_rate_limit_reads.insert(req.id);
                    let _ = send_line(&mut stdin, &codex_proto::to_ndjson_request(&req), &mut transcript);
                }
                Ok(CodexDriverCommand::Shutdown) => {
                    eprintln!("[codex] agent {agent_id}: shutdown requested");
                    if let Ok(mut c) = child.lock() {
                        let _ = c.kill();
                    }
                    should_exit = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    eprintln!("[codex] agent {agent_id}: command channel closed");
                    if let Ok(mut c) = child.lock() {
                        let _ = c.kill();
                    }
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

// ---------------------------------------------------------------------------
// Notification handler
// ---------------------------------------------------------------------------

fn update_codex_from_payload(state: &Arc<Mutex<SupervisorState>>, payload: &Value) {
    let limits = payload.get("rateLimits").unwrap_or(payload);
    let primary = limits.get("primary").and_then(parse_codex_limit);
    let secondary = limits.get("secondary").and_then(parse_codex_limit);
    update_codex_rate_limits(state, primary, secondary);
}

fn parse_codex_limit(v: &Value) -> Option<(u32, Option<String>)> {
    let pct = v
        .get("usedPercent")
        .or_else(|| v.get("utilization"))
        .and_then(|n| n.as_f64().or_else(|| n.as_u64().map(|x| x as f64)))
        .map(|n| n.round().clamp(0.0, 100.0) as u32)?;

    let reset = v
        .get("resetsAt")
        .or_else(|| v.get("resets_at"))
        .and_then(|n| n.as_i64().or_else(|| n.as_u64().map(|x| x as i64)))
        .and_then(format_reset_label);

    Some((pct, reset))
}

fn format_reset_label(unix_secs: i64) -> Option<String> {
    let dt = chrono::Local.timestamp_opt(unix_secs, 0).single()?;
    Some(format!("Resets {}", dt.format("%H:%M on %-d %b")))
}

fn handle_notification(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    method: &str,
    params: &Value,
) {
    match method {
        "item/agentMessage/delta" => {
            if let Some(delta) = codex_proto::extract_delta(params) {
                emit_output(state, agent_id, &codex_render::render_text_delta(&delta));
            }
        }
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
            if let Some(delta) = codex_proto::extract_delta(params) {
                emit_output(state, agent_id, &codex_render::render_thinking(&delta));
            }
        }
        "item/commandExecution/outputDelta" => {
            if let Some(delta) = codex_proto::extract_delta(params) {
                emit_output(state, agent_id, &codex_render::render_command_output(&delta));
            }
        }
        "item/fileChange/outputDelta" => {
            if let Some(delta) = codex_proto::extract_delta(params) {
                emit_output(state, agent_id, &codex_render::render_file_change(&delta));
            }
        }
        "item/started" => {
            if let Some(item) = params.get("item") {
                let item_type = item
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if item_type != "agentMessage" {
                    let tool_name = item_type.to_string();
                    emit_output(state, agent_id, &codex_render::render_tool_header(&tool_name));
                    emit_event(
                        state,
                        agent_id,
                        AgentEventType::ToolStart { tool_name },
                    );
                }
            }
        }
        "item/completed" => {
            if let Some(item) = params.get("item") {
                let item_type = item
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if item_type != "agentMessage" {
                    emit_event(
                        state,
                        agent_id,
                        AgentEventType::ToolEnd {
                            tool_name: item_type.to_string(),
                            success: true,
                            duration_ms: 0,
                        },
                    );
                }
                if item_type == "agentMessage" {
                    emit_output(state, agent_id, &codex_render::render_message_separator());
                }
            }
        }
        "turn/started" => {
            emit_event(state, agent_id, AgentEventType::Thinking);
        }
        "turn/completed" => {
            // Check if turn/completed carries token usage
            if let Some(usage) = params.get("usage").or_else(|| params.get("tokenUsage")) {
                eprintln!("[codex] agent {agent_id}: turn/completed has usage: {usage}");
                let input = usage
                    .get("inputTokens")
                    .or_else(|| usage.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output = usage
                    .get("outputTokens")
                    .or_else(|| usage.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if input > 0 || output > 0 {
                    emit_event(
                        state,
                        agent_id,
                        AgentEventType::TokenUsage {
                            input_tokens: input,
                            output_tokens: output,
                        },
                    );
                }
            }
            emit_output(state, agent_id, &codex_render::render_turn_complete());
            emit_event(
                state,
                agent_id,
                AgentEventType::QueryComplete {
                    num_turns: 1,
                    duration_ms: 0,
                    is_error: false,
                },
            );
            emit_event(state, agent_id, AgentEventType::WaitingForInput);
        }
        "thread/started" => {
            eprintln!("[codex] agent {agent_id}: thread/started notification");
        }
        "thread/compacted" => {
            emit_output(state, agent_id, &codex_render::render_compacted());
        }
        "thread/tokenUsage/updated" => {
            eprintln!("[codex] agent {agent_id}: tokenUsage params={params}");
            let input = params
                .get("usage")
                .or(Some(params))
                .and_then(|u| u.get("inputTokens").or_else(|| u.get("input_tokens")))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = params
                .get("usage")
                .or(Some(params))
                .and_then(|u| u.get("outputTokens").or_else(|| u.get("output_tokens")))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if input > 0 || output > 0 {
                emit_event(
                    state,
                    agent_id,
                    AgentEventType::TokenUsage {
                        input_tokens: input,
                        output_tokens: output,
                    },
                );
            }
        }
        "account/rateLimits/updated" => {
            update_codex_from_payload(state, params);
            eprintln!("[codex] agent {agent_id}: rate limits updated");
        }
        "error" => {
            let msg = params
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            let will_retry = params
                .get("willRetry")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            emit_output(state, agent_id, &codex_render::render_error(msg, will_retry));
            if !will_retry {
                emit_event(
                    state,
                    agent_id,
                    AgentEventType::Error {
                        message: msg.to_string(),
                    },
                );
            }
        }
        "codex/connected" => {
            eprintln!("[codex] agent {agent_id}: connected");
        }
        _ => {
            eprintln!("[codex] agent {agent_id}: unhandled notification: {method} params={params}");
        }
    }
}

// ---------------------------------------------------------------------------
// Approval handling
// ---------------------------------------------------------------------------

/// Handle a Codex server request that requires a JSON-RPC response.
fn handle_request_requiring_response(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    request_id: Value,
    method: &str,
    params: &Value,
) -> Option<PendingToolApproval> {
    let kind = classify_pending_request_kind(method)?;

    // Extract a readable tool/request name for UI display.
    let tool_name = match kind {
        PendingCodexRequestKind::DynamicToolCall => params
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("DynamicToolCall")
            .to_string(),
        PendingCodexRequestKind::UserInput => "AskUserQuestion".to_string(),
        PendingCodexRequestKind::ReviewDecision => method.to_string(),
        PendingCodexRequestKind::StandardApproval => {
            let name = method
                .strip_prefix("item/")
                .unwrap_or(method)
                .replace("requestApproval", "")
                .replace("request_approval", "");
            if name.is_empty() { "tool".to_string() } else { name }
        }
    };

    emit_output(
        state,
        agent_id,
        &codex_render::render_approval_request(&tool_name, params),
    );

    // Broadcast approval request to UI (dashboard shows deny button)
    broadcast(
        state,
        &ServerMessage::ToolApprovalRequest {
            agent_id,
            request_id: request_id.to_string(),
            tool_name: tool_name.clone(),
            tool_input: params.clone(),
            description: None,
        },
    );

    eprintln!(
        "[codex] agent {agent_id}: pending request {tool_name} kind={:?}",
        kind
    );

    Some(PendingToolApproval {
        request_id_key: request_id.to_string(),
        request_id,
        tool_name,
        kind,
    })
}

fn classify_pending_request_kind(method: &str) -> Option<PendingCodexRequestKind> {
    match method {
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/mcpToolCall/requestApproval" => Some(PendingCodexRequestKind::StandardApproval),
        "item/tool/requestUserInput" => Some(PendingCodexRequestKind::UserInput),
        "applyPatchApproval" | "execCommandApproval" => Some(PendingCodexRequestKind::ReviewDecision),
        "item/tool/call" => Some(PendingCodexRequestKind::DynamicToolCall),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn send_line(
    stdin: &mut std::process::ChildStdin,
    line: &str,
    transcript: &mut Option<Transcript>,
) -> io::Result<()> {
    if let Some(ref mut t) = transcript {
        t.log_outgoing(line);
    }
    stdin.write_all(line.as_bytes())?;
    stdin.flush()
}
