use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::Value;

use chrono::Utc;

use crate::agent::status::AgentStatus;
use crate::agent::types::AgentType;
use crate::config;
use crate::ipc::framing::{read_message, write_message};
use crate::ipc::proto::{AgentInfo, ClientMessage, ServerMessage};
use crate::persistence::db::AgentStore;

use super::codex_driver::CodexDriverCommand;
use super::opencode_driver::OpenCodeDriverCommand;
use super::md_log::{ConversationMdLog, MdLogMode};
use super::session_catalog;

const PROTOCOL_VERSION: u32 = 1;
const RECENT_SESSION_GROUP_LIMIT: usize = 5;

/// Trait for sending messages to a connected client, regardless of transport.
pub(crate) trait ClientWriter: Send {
    fn send_message(&mut self, msg: &ServerMessage) -> io::Result<()>;
}

/// Unix socket client writer — uses length-prefixed JSON framing.
struct UnixClientWriter {
    stream: UnixStream,
}

impl ClientWriter for UnixClientWriter {
    fn send_message(&mut self, msg: &ServerMessage) -> io::Result<()> {
        write_message(&mut self.stream, msg)
    }
}

pub(crate) struct SupervisorState {
    pub(crate) next_id: u32,
    pub(crate) agents: HashMap<u32, AgentRuntime>,
    pub(crate) clients: Vec<Arc<Mutex<dyn ClientWriter>>>,
    /// WebSocket configuration
    pub(crate) ws_enabled: bool,
    pub(crate) ws_password: String,
    pub(crate) ws_port: u16,
    /// Connected WebSocket peer IPs
    pub(crate) ws_peers: Vec<String>,
    /// Persistence store
    pub(crate) store: Option<AgentStore>,
    /// WebSocket listener shutdown flag (shared with listener thread)
    pub(crate) ws_shutdown: Arc<AtomicBool>,
    /// Whether the WS listener is currently running
    pub(crate) ws_listener_running: bool,
    /// Latest usage poll result
    pub(crate) usage_info: Option<UsageInfo>,
}

/// Result from polling `claude /usage` (or file-based fallback).
#[derive(Clone)]
pub(crate) struct UsageInfo {
    pub raw_output: String,
    /// Real percentages from `/usage` probe
    pub session_percent: Option<u32>,
    pub session_reset: Option<String>,
    pub week_all_percent: Option<u32>,
    pub week_all_reset: Option<String>,
    pub week_sonnet_percent: Option<u32>,
    pub week_sonnet_reset: Option<String>,
    pub plan_tier: Option<String>,
    /// File-based fallback fields
    pub session_messages: Option<u32>,
    pub session_limit: Option<u32>,
    pub daily_messages: Option<u32>,
    pub weekly_messages: Option<u32>,
    pub messages_used: Option<u32>,
    pub messages_limit: Option<u32>,
    /// Codex account limits from `account/rateLimits/*`.
    pub codex_five_hour_percent: Option<u32>,
    pub codex_five_hour_reset: Option<String>,
    pub codex_weekly_percent: Option<u32>,
    pub codex_weekly_reset: Option<String>,
}

impl SupervisorState {
    pub(crate) fn new() -> Self {
        let config = crate::config::load_config();

        // Open persistence store and get max ID
        let (store, next_id) = match AgentStore::open() {
            Ok(s) => {
                let max_id = s.max_id().unwrap_or(0);
                eprintln!("[persistence] opened database, max agent id = {}", max_id);
                (Some(s), max_id + 1)
            }
            Err(e) => {
                eprintln!("[persistence] failed to open database: {e}");
                (None, 0)
            }
        };

        Self {
            next_id,
            agents: HashMap::new(),
            clients: Vec::new(),
            ws_enabled: config.ws_enabled,
            ws_password: config.ws_password,
            ws_port: config.ws_port,
            ws_peers: Vec::new(),
            store,
            ws_shutdown: Arc::new(AtomicBool::new(false)),
            ws_listener_running: false,
            usage_info: None,
        }
    }

    pub(crate) fn snapshot_agents(&self) -> Vec<AgentInfo> {
        self.agents
            .values()
            .map(|agent| {
                let mut info = agent.info.clone();
                info.session_id = agent.session_id.clone();
                info.thread_id = agent.thread_id.clone();
                info
            })
            .collect()
    }
}

/// Commands sent to the Claude WS driver thread.
pub(crate) enum ClaudeDriverCommand {
    SendPrompt { prompt: String },
    ToolApprovalResponse {
        request_id: String,
        approved: bool,
        updated_input: Option<Value>,
    },
    Interrupt,
    Shutdown,
}

/// Backend-specific state for an agent.
pub(crate) enum AgentBackend {
    /// PTY-based agent (Codex, OpenCode, or Claude without WS).
    Pty {
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
        master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    },
    /// Claude Code via `--sdk-url` WebSocket protocol.
    ClaudeWs {
        child: Arc<Mutex<std::process::Child>>,
        command_tx: mpsc::Sender<ClaudeDriverCommand>,
    },
    /// Codex via `app-server` JSON-RPC over stdin/stdout.
    CodexStdio {
        child: Arc<Mutex<std::process::Child>>,
        command_tx: mpsc::Sender<CodexDriverCommand>,
    },
    /// OpenCode via `serve` HTTP + SSE.
    OpenCodeHttp {
        child: Arc<Mutex<std::process::Child>>,
        command_tx: mpsc::Sender<OpenCodeDriverCommand>,
    },
}

pub(crate) struct AgentRuntime {
    pub(crate) info: AgentInfo,
    pub(crate) status: AgentStatus,
    pub(crate) prompt: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) thread_id: Option<String>,
    /// OS PID of the top-level agent process (the session leader after setsid).
    /// Used to kill the entire process group on demand or at supervisor startup
    /// to reap orphans from a previous run.
    pub(crate) child_pid: Option<u32>,
    pub(crate) backend: AgentBackend,
    pub(crate) output_buffer: Vec<u8>,
    pub(crate) output_paused: bool,
    pub(crate) md_log: Option<ConversationMdLog>,
}

pub fn run(socket_path: &Path) -> io::Result<()> {
    if let Some(parent) = socket_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;

    // Write PID file so the UI can kill us if we become stale
    let pid_path = socket_path.with_extension("pid");
    fs::write(&pid_path, std::process::id().to_string())?;

    let state = Arc::new(Mutex::new(SupervisorState::new()));

    // Spawn WebSocket listener if enabled in config or via env var
    let ws_enabled = {
        let s = state.lock().unwrap();
        s.ws_enabled || std::env::var("OPENSWARM_WS_TOKEN").is_ok()
    };
    if ws_enabled {
        start_ws_listener(&state);
    }

    // Start usage polling (reads local Claude credential/stats files)
    super::usage_poll::start(state.clone());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                thread::spawn(move || handle_client(stream, state));
            }
            Err(e) => {
                eprintln!("Supervisor accept error: {e}");
            }
        }
    }

    Ok(())
}

fn handle_client(stream: UnixStream, state: Arc<Mutex<SupervisorState>>) {
    let reader = match stream.try_clone() {
        Ok(r) => r,
        Err(_) => return,
    };

    let writer: Arc<Mutex<dyn ClientWriter>> =
        Arc::new(Mutex::new(UnixClientWriter { stream }));

    register_and_welcome(&state, &writer);
    client_message_loop(reader, &state, &writer);
    remove_client(&state, &writer);
}

/// Register a client, send Welcome, AgentList, and historical output.
pub(crate) fn register_and_welcome(
    state: &Arc<Mutex<SupervisorState>>,
    writer: &Arc<Mutex<dyn ClientWriter>>,
) {
    {
        let mut state = match state.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.clients.push(writer.clone());
    }

    // Welcome
    {
        let mut w = writer.lock().unwrap();
        let _ = w.send_message(&ServerMessage::Welcome {
            protocol: PROTOCOL_VERSION,
            build_id: crate::ipc::build_id(),
        });
    }

    // Model catalog for spawn UIs
    {
        let mut w = writer.lock().unwrap();
        let _ = w.send_message(&ServerMessage::ModelCatalog {
            catalogs: config::model_catalog(),
        });
    }

    // Agent list + historical output
    {
        let (agents, history) = {
            let state = state.lock().unwrap();
            let agents = state.snapshot_agents();
            let history = state
                .agents
                .values()
                .filter(|agent| !agent.output_buffer.is_empty())
                .map(|agent| (agent.info.id, agent.output_buffer.clone()))
                .collect::<Vec<_>>();
            (agents, history)
        };
        let mut w = writer.lock().unwrap();
        let _ = w.send_message(&ServerMessage::AgentList { agents });
        for (agent_id, bytes) in history {
            let _ = w.send_message(&ServerMessage::AgentOutput { agent_id, bytes });
        }
    }

    // Recent sessions (SQLite-backed)
    {
        let repos = session_catalog::repo_inventory_snapshot();
        let sessions = session_catalog::recent_sessions_snapshot(state, RECENT_SESSION_GROUP_LIMIT);
        let mut w = writer.lock().unwrap();
        let _ = w.send_message(&ServerMessage::RepoInventory { repos });
        let _ = w.send_message(&ServerMessage::RecentSessions { sessions });
    }

    // Send current usage status if available
    {
        let s = state.lock().unwrap();
        let _ = writer.lock().unwrap().send_message(&ServerMessage::WsStatus {
            enabled: s.ws_enabled,
            connected_peers: s.ws_peers.clone(),
        });
        if let Some(ref info) = s.usage_info {
            let mut w = writer.lock().unwrap();
            let _ = w.send_message(&usage_status_message(info));
        }
    }
}

/// Process ClientMessages from a reader, dispatching to shared state.
pub(crate) fn dispatch_client_message(
    msg: ClientMessage,
    state: &Arc<Mutex<SupervisorState>>,
) {
    match msg {
        ClientMessage::SpawnAgent {
            agent_type,
            repo_path,
            prompt,
            parent_id,
            model,
        } => {
            if let Err(e) = spawn_agent(state, agent_type, repo_path, prompt, parent_id, model) {
                eprintln!("Spawn agent error: {e}");
            }
        }
        ClientMessage::KillAgent { agent_id } => {
            kill_agent(state, agent_id);
        }
        ClientMessage::Input { agent_id, bytes } => {
            send_input(state, agent_id, &bytes);
        }
        ClientMessage::ResizeAgent {
            agent_id,
            rows,
            cols,
        } => {
            resize_agent(state, agent_id, rows, cols);
        }
        ClientMessage::SetOutputPaused { agent_id, paused } => {
            set_output_paused(state, agent_id, paused);
        }
        ClientMessage::SetWsConfig { enabled, password } => {
            set_ws_config(state, enabled, password);
        }
        ClientMessage::GetWsStatus => {
            broadcast_ws_status(state);
        }
        ClientMessage::ResumeAgent { agent_id } => {
            if let Err(e) = resume_agent(state, agent_id) {
                eprintln!("Resume agent error: {e}");
            }
        }
        ClientMessage::ResumeExportedSession {
            agent_type,
            repo_path,
            session_handle,
        } => {
            if let Err(e) = resume_exported_session(state, agent_type, repo_path, session_handle) {
                eprintln!("Resume exported session error: {e}");
            }
        }
        ClientMessage::SendPrompt { agent_id, prompt } => {
            dispatch_send_prompt(state, agent_id, prompt);
        }
        ClientMessage::ToolApprovalResponse {
            agent_id,
            request_id,
            approved,
            updated_input,
        } => {
            dispatch_tool_approval(state, agent_id, request_id, approved, updated_input);
        }
        ClientMessage::Interrupt { agent_id } => {
            dispatch_interrupt(state, agent_id);
        }
        ClientMessage::QuestionResponse {
            agent_id,
            request_id,
            answers,
            rejected,
        } => {
            dispatch_question_response(state, agent_id, request_id, answers, rejected);
        }
        ClientMessage::RefreshUsage => {
            super::usage_poll::refresh_now(state.clone());
            let codex_channels = {
                let st = state.lock().unwrap();
                st.agents
                    .values()
                    .filter_map(|agent| match &agent.backend {
                        AgentBackend::CodexStdio { command_tx, .. } => Some(command_tx.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            for tx in codex_channels {
                let _ = tx.send(CodexDriverCommand::RefreshUsage);
            }
        }
        ClientMessage::Ack { .. } => {
            // Handled by the WebSocket transport loop for per-client seq/ack tracking.
        }
    }
}

/// Read loop for Unix socket clients.
fn client_message_loop(
    mut reader: UnixStream,
    state: &Arc<Mutex<SupervisorState>>,
    _writer: &Arc<Mutex<dyn ClientWriter>>,
) {
    loop {
        let msg = match read_message::<ClientMessage>(&mut reader) {
            Ok(msg) => msg,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                eprintln!("Supervisor: skipping unrecognised client message");
                continue;
            }
            Err(_) => break,
        };
        dispatch_client_message(msg, state);
    }
}

pub(crate) fn remove_client(
    state: &Arc<Mutex<SupervisorState>>,
    writer: &Arc<Mutex<dyn ClientWriter>>,
) {
    if let Ok(mut state) = state.lock() {
        state
            .clients
            .retain(|client| !Arc::ptr_eq(client, writer));
    }
}

pub(crate) fn broadcast(state: &Arc<Mutex<SupervisorState>>, msg: &ServerMessage) {
    let clients = {
        let state = match state.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.clients.clone()
    };

    for client in clients {
        if let Ok(mut writer) = client.lock() {
            if writer.send_message(msg).is_err() {
                remove_client(state, &client);
            }
        }
    }
}

fn open_md_log(
    agent_id: u32,
    agent_type: AgentType,
    repo_name: &str,
    repo_path: &str,
    mode: MdLogMode,
) -> Option<ConversationMdLog> {
    match ConversationMdLog::open(agent_id, agent_type, repo_name, repo_path, mode) {
        Ok(log) => Some(log),
        Err(e) => {
            eprintln!("[md-log] agent {agent_id}: failed to open markdown log: {e}");
            None
        }
    }
}

fn spawn_agent(
    state: &Arc<Mutex<SupervisorState>>,
    agent_type: AgentType,
    repo_path: String,
    prompt: Option<String>,
    parent_id: Option<u32>,
    model: Option<String>,
) -> io::Result<()> {
    let id = {
        let mut state = state.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        id
    };

    let repo_name = Path::new(&repo_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let canonical_repo = std::fs::canonicalize(&repo_path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| repo_path.clone());

    // Branch: Claude Code uses the WS driver; Codex uses stdio JSON-RPC; others use PTY
    if agent_type == AgentType::ClaudeCode {
        return spawn_claude_code_ws(state, id, parent_id, agent_type, repo_path, repo_name, &canonical_repo, prompt, None, model);
    }
    if agent_type == AgentType::Codex {
        return spawn_codex_driver(state, id, parent_id, agent_type, repo_path, repo_name, &canonical_repo, prompt, None, model);
    }
    if agent_type == AgentType::OpenCode {
        return spawn_opencode_driver(state, id, parent_id, agent_type, repo_path, repo_name, &canonical_repo, prompt, model);
    }

    spawn_pty_agent(state, id, parent_id, agent_type, repo_path, repo_name, &canonical_repo, prompt)
}

fn resume_exported_session(
    state: &Arc<Mutex<SupervisorState>>,
    agent_type: AgentType,
    repo_path: String,
    session_handle: String,
) -> io::Result<()> {
    let canonical_repo = std::fs::canonicalize(&repo_path)
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("repo not available: {e}")))?;
    let canonical_repo = canonical_repo.to_string_lossy().to_string();
    let repo_name = Path::new(&canonical_repo)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let id = {
        let mut state = state.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        id
    };

    match agent_type {
        AgentType::ClaudeCode => spawn_claude_code_ws(
            state,
            id,
            None,
            agent_type,
            repo_path,
            repo_name,
            &canonical_repo,
            None,
            Some(session_handle),
            None,
        ),
        AgentType::Codex => spawn_codex_driver(
            state,
            id,
            None,
            agent_type,
            repo_path,
            repo_name,
            &canonical_repo,
            None,
            Some(session_handle),
            None,
        ),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("agent type {} does not support exported-session resume", agent_type.label()),
        )),
    }
}

/// Spawn Claude Code via the `--sdk-url` WebSocket protocol.
fn spawn_claude_code_ws(
    state: &Arc<Mutex<SupervisorState>>,
    id: u32,
    parent_id: Option<u32>,
    agent_type: AgentType,
    repo_path: String,
    repo_name: String,
    canonical_repo: &str,
    prompt: Option<String>,
    resume_session_id: Option<String>,
    model: Option<String>,
) -> io::Result<()> {
    let (child, command_tx, child_pid) =
        super::claude_ws::spawn_claude_ws(state, id, canonical_repo, prompt.clone(), resume_session_id, model)?;

    let info = AgentInfo {
        id,
        parent_id,
        agent_type,
        repo_path,
        repo_name,
        status: AgentStatus::Running,
        session_id: None,
        thread_id: None,
    };

    let runtime = AgentRuntime {
        info: info.clone(),
        status: AgentStatus::Running,
        prompt: prompt.clone(),
        session_id: None,
        thread_id: None,
        child_pid: Some(child_pid),
        backend: AgentBackend::ClaudeWs { child, command_tx },
        output_buffer: Vec::new(),
        output_paused: false,
        md_log: None,
    };

    {
        let mut state = state.lock().unwrap();
        state.agents.insert(id, runtime);

        if let Some(ref store) = state.store {
            let persisted = crate::persistence::db::PersistedAgent {
                id,
                parent_id,
                agent_type,
                repo_path: info.repo_path.clone(),
                prompt: prompt.clone(),
                spawn_time: Utc::now(),
                exit_time: None,
                last_status: AgentStatus::Running,
                output_tail: None,
                session_id: None,
                thread_id: None,
            };
            if let Err(e) = store.persist(&persisted) {
                eprintln!("[persistence] failed to persist agent {}: {}", id, e);
            }
        }
    }

    broadcast(state, &ServerMessage::AgentList { agents: snapshot(state) });
    broadcast(
        state,
        &ServerMessage::RecentSessions {
            sessions: session_catalog::recent_sessions_snapshot(state, RECENT_SESSION_GROUP_LIMIT),
        },
    );
    Ok(())
}

/// Spawn Codex via `app-server` JSON-RPC over stdin/stdout.
fn spawn_codex_driver(
    state: &Arc<Mutex<SupervisorState>>,
    id: u32,
    parent_id: Option<u32>,
    agent_type: AgentType,
    repo_path: String,
    repo_name: String,
    canonical_repo: &str,
    prompt: Option<String>,
    resume_thread_id: Option<String>,
    model: Option<String>,
) -> io::Result<()> {
    let (child, command_tx, child_pid) =
        super::codex_driver::spawn_codex(state, id, canonical_repo, prompt.clone(), resume_thread_id, model)?;

    let info = AgentInfo {
        id,
        parent_id,
        agent_type,
        repo_path,
        repo_name,
        status: AgentStatus::Running,
        session_id: None,
        thread_id: None,
    };

    let runtime = AgentRuntime {
        info: info.clone(),
        status: AgentStatus::Running,
        prompt: prompt.clone(),
        session_id: None,
        thread_id: None,
        child_pid: Some(child_pid),
        backend: AgentBackend::CodexStdio { child, command_tx },
        output_buffer: Vec::new(),
        output_paused: false,
        md_log: None,
    };

    {
        let mut state = state.lock().unwrap();
        state.agents.insert(id, runtime);

        if let Some(ref store) = state.store {
            let persisted = crate::persistence::db::PersistedAgent {
                id,
                parent_id,
                agent_type,
                repo_path: info.repo_path.clone(),
                prompt: prompt.clone(),
                spawn_time: Utc::now(),
                exit_time: None,
                last_status: AgentStatus::Running,
                output_tail: None,
                session_id: None,
                thread_id: None,
            };
            if let Err(e) = store.persist(&persisted) {
                eprintln!("[persistence] failed to persist agent {}: {}", id, e);
            }
        }
    }

    broadcast(state, &ServerMessage::AgentList { agents: snapshot(state) });
    broadcast(
        state,
        &ServerMessage::RecentSessions {
            sessions: session_catalog::recent_sessions_snapshot(state, RECENT_SESSION_GROUP_LIMIT),
        },
    );
    Ok(())
}

/// Spawn OpenCode via `serve` HTTP + SSE driver.
fn spawn_opencode_driver(
    state: &Arc<Mutex<SupervisorState>>,
    id: u32,
    parent_id: Option<u32>,
    agent_type: AgentType,
    repo_path: String,
    repo_name: String,
    canonical_repo: &str,
    prompt: Option<String>,
    model: Option<String>,
) -> io::Result<()> {
    let (child, command_tx, child_pid) =
        super::opencode_driver::spawn_opencode(state, id, canonical_repo, prompt.clone(), model.clone())?;

    let info = AgentInfo {
        id,
        parent_id,
        agent_type,
        repo_path,
        repo_name,
        status: AgentStatus::Running,
        session_id: None,
        thread_id: None,
    };

    let runtime = AgentRuntime {
        info: info.clone(),
        status: AgentStatus::Running,
        prompt: prompt.clone(),
        session_id: None,
        thread_id: None,
        child_pid: Some(child_pid),
        backend: AgentBackend::OpenCodeHttp { child, command_tx },
        output_buffer: Vec::new(),
        output_paused: false,
        md_log: None,
    };

    {
        let mut state = state.lock().unwrap();
        state.agents.insert(id, runtime);

        if let Some(ref store) = state.store {
            let persisted = crate::persistence::db::PersistedAgent {
                id,
                parent_id,
                agent_type,
                repo_path: info.repo_path.clone(),
                prompt: prompt.clone(),
                spawn_time: Utc::now(),
                exit_time: None,
                last_status: AgentStatus::Running,
                output_tail: None,
                session_id: None,
                thread_id: None,
            };
            if let Err(e) = store.persist(&persisted) {
                eprintln!("[persistence] failed to persist agent {}: {}", id, e);
            }
        }
    }

    broadcast(state, &ServerMessage::AgentList { agents: snapshot(state) });
    broadcast(
        state,
        &ServerMessage::RecentSessions {
            sessions: session_catalog::recent_sessions_snapshot(state, RECENT_SESSION_GROUP_LIMIT),
        },
    );
    Ok(())
}

/// Spawn an agent via PTY (OpenCode, or fallback).
fn spawn_pty_agent(
    state: &Arc<Mutex<SupervisorState>>,
    id: u32,
    parent_id: Option<u32>,
    agent_type: AgentType,
    repo_path: String,
    repo_name: String,
    canonical_repo: &str,
    prompt: Option<String>,
) -> io::Result<()> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: crate::config::DEFAULT_PTY_ROWS,
            cols: crate::config::DEFAULT_PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let mut cmd = CommandBuilder::new(agent_type.command());
    if agent_type == AgentType::Codex {
        cmd.arg("--no-alt-screen");
    }
    if agent_type == AgentType::OpenCode {
        cmd.arg(canonical_repo);
        cmd.arg("--model");
        cmd.arg(crate::config::OPENCODE_DEFAULT_MODEL);
    }

    if let Some(ref prompt) = prompt {
        if !prompt.is_empty() {
            if agent_type == AgentType::OpenCode {
                cmd.arg("--prompt");
                cmd.arg(prompt);
            } else {
                cmd.arg(prompt);
            }
        }
    }
    cmd.cwd(canonical_repo);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let master = pair.master;

    let child_pid = child.process_id();
    // Coerce away the Sync bound so the type matches AgentBackend::Pty.
    let child: Box<dyn portable_pty::Child + Send> = child;

    let info = AgentInfo {
        id,
        parent_id,
        agent_type,
        repo_path,
        repo_name,
        status: AgentStatus::Running,
        session_id: None,
        thread_id: None,
    };

    let child = Arc::new(Mutex::new(child));
    let mut md_log = open_md_log(
        id,
        agent_type,
        &info.repo_name,
        &info.repo_path,
        MdLogMode::RenderedOutputFallback,
    );
    if let Some(p) = prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        if let Some(log) = md_log.as_mut() {
            log.append_user_prompt(p);
        }
    }

    let runtime = AgentRuntime {
        info: info.clone(),
        status: AgentStatus::Running,
        prompt: prompt.clone(),
        session_id: None,
        thread_id: None,
        child_pid,
        backend: AgentBackend::Pty {
            writer: Arc::new(Mutex::new(writer)),
            child,
            master: Arc::new(Mutex::new(master)),
        },
        output_buffer: Vec::new(),
        output_paused: false,
        md_log,
    };

    {
        let mut state = state.lock().unwrap();
        state.agents.insert(id, runtime);

        if let Some(ref store) = state.store {
            let persisted = crate::persistence::db::PersistedAgent {
                id,
                parent_id,
                agent_type,
                repo_path: info.repo_path.clone(),
                prompt: prompt.clone(),
                spawn_time: Utc::now(),
                exit_time: None,
                last_status: AgentStatus::Running,
                output_tail: None,
                session_id: None,
                thread_id: None,
            };
            if let Err(e) = store.persist(&persisted) {
                eprintln!("[persistence] failed to persist agent {}: {}", id, e);
            }
        }
    }

    broadcast(state, &ServerMessage::AgentList { agents: snapshot(state) });
    broadcast(
        state,
        &ServerMessage::RecentSessions {
            sessions: session_catalog::recent_sessions_snapshot(state, RECENT_SESSION_GROUP_LIMIT),
        },
    );

    // Output reader thread
    let state_clone = state.clone();
    let parser_agent_type = agent_type;
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut parser = super::parser::EventParser::new(parser_agent_type);
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = &buf[..n];
                    append_output(&state_clone, id, bytes);

                    for event in parser.feed(bytes) {
                        let timestamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        broadcast(
                            &state_clone,
                            &ServerMessage::AgentEvent {
                                agent_id: id,
                                timestamp,
                                event,
                            },
                        );
                    }

                    let paused = state_clone
                        .lock()
                        .map(|s| s.agents.get(&id).map_or(false, |a| a.output_paused))
                        .unwrap_or(false);
                    if !paused {
                        broadcast(
                            &state_clone,
                            &ServerMessage::AgentOutput {
                                agent_id: id,
                                bytes: bytes.to_vec(),
                            },
                        );
                    }
                }
                Err(_) => break,
            }
        }
        set_status(&state_clone, id, AgentStatus::Exited);

        {
            let state = state_clone.lock().unwrap();
            if let Some(ref store) = state.store {
                let output_tail = state.agents.get(&id)
                    .map(|a| {
                        const TAIL_SIZE: usize = 32 * 1024;
                        if a.output_buffer.len() > TAIL_SIZE {
                            a.output_buffer[a.output_buffer.len() - TAIL_SIZE..].to_vec()
                        } else {
                            a.output_buffer.clone()
                        }
                    })
                    .unwrap_or_default();
                if let Err(e) = store.mark_exited(id, &output_tail) {
                    eprintln!("[persistence] failed to mark agent {} as exited: {}", id, e);
                }
            }
        }

        broadcast(
            &state_clone,
            &ServerMessage::AgentStatus {
                agent_id: id,
                status: AgentStatus::Exited,
            },
        );
    });

    Ok(())
}

pub(crate) fn append_output(state: &Arc<Mutex<SupervisorState>>, agent_id: u32, bytes: &[u8]) {
    if let Ok(mut state) = state.lock() {
        if let Some(agent) = state.agents.get_mut(&agent_id) {
            agent.output_buffer.extend_from_slice(bytes);
            let max_size = agent.info.agent_type.output_buffer_size();
            if agent.output_buffer.len() > max_size {
                let excess = agent.output_buffer.len() - max_size;
                agent.output_buffer.drain(0..excess);
            }
            if let Some(ref mut md_log) = agent.md_log {
                if md_log.mode() == MdLogMode::RenderedOutputFallback {
                    md_log.append_rendered_output_chunk(bytes);
                }
            }
        }
    }
}

pub(crate) fn set_status(state: &Arc<Mutex<SupervisorState>>, agent_id: u32, status: AgentStatus) {
    if let Ok(mut state) = state.lock() {
        if let Some(agent) = state.agents.get_mut(&agent_id) {
            agent.status = status;
            agent.info.status = status;
        }
    }
}

pub(crate) fn emit_event(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    event: crate::ipc::proto::AgentEventType,
) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    broadcast(
        state,
        &ServerMessage::AgentEvent {
            agent_id,
            timestamp,
            event,
        },
    );
}

pub(crate) fn usage_status_message(info: &UsageInfo) -> ServerMessage {
    ServerMessage::UsageStatus {
        raw_output: info.raw_output.clone(),
        session_percent: info.session_percent,
        session_reset: info.session_reset.clone(),
        week_all_percent: info.week_all_percent,
        week_all_reset: info.week_all_reset.clone(),
        week_sonnet_percent: info.week_sonnet_percent,
        week_sonnet_reset: info.week_sonnet_reset.clone(),
        session_messages: info.session_messages,
        session_limit: info.session_limit,
        daily_messages: info.daily_messages,
        weekly_messages: info.weekly_messages,
        messages_used: info.messages_used,
        messages_limit: info.messages_limit,
        plan_tier: info.plan_tier.clone(),
        codex_five_hour_percent: info.codex_five_hour_percent,
        codex_five_hour_reset: info.codex_five_hour_reset.clone(),
        codex_weekly_percent: info.codex_weekly_percent,
        codex_weekly_reset: info.codex_weekly_reset.clone(),
    }
}

pub(crate) fn update_codex_rate_limits(
    state: &Arc<Mutex<SupervisorState>>,
    five_hour: Option<(u32, Option<String>)>,
    weekly: Option<(u32, Option<String>)>,
) {
    let msg = {
        let mut st = match state.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };

        let info = st.usage_info.get_or_insert_with(|| UsageInfo {
            raw_output: String::new(),
            session_percent: None,
            session_reset: None,
            week_all_percent: None,
            week_all_reset: None,
            week_sonnet_percent: None,
            week_sonnet_reset: None,
            plan_tier: None,
            session_messages: None,
            session_limit: None,
            daily_messages: None,
            weekly_messages: None,
            messages_used: None,
            messages_limit: None,
            codex_five_hour_percent: None,
            codex_five_hour_reset: None,
            codex_weekly_percent: None,
            codex_weekly_reset: None,
        });

        info.codex_five_hour_percent = five_hour.as_ref().map(|(pct, _)| *pct);
        info.codex_five_hour_reset = five_hour.and_then(|(_, reset)| reset);
        info.codex_weekly_percent = weekly.as_ref().map(|(pct, _)| *pct);
        info.codex_weekly_reset = weekly.and_then(|(_, reset)| reset);
        info.raw_output = format!(
            "Codex limits: 5h={}%, weekly={}%",
            info.codex_five_hour_percent.unwrap_or(0),
            info.codex_weekly_percent.unwrap_or(0),
        );

        usage_status_message(info)
    };

    broadcast(state, &msg);
}

fn kill_agent(state: &Arc<Mutex<SupervisorState>>, agent_id: u32) {
    if let Ok(state) = state.lock() {
        if let Some(agent) = state.agents.get(&agent_id) {
            match &agent.backend {
                AgentBackend::Pty { child, .. } => {
                    if let Ok(mut child) = child.lock() {
                        // All PTY children: kill entire process group.
                        // PTY subsystem puts the child in its own session,
                        // so PGID == child PID kills the full tree.
                        if let Some(pid) = child.process_id() {
                            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                        } else {
                            let _ = child.kill();
                        }
                    }
                }
                AgentBackend::ClaudeWs { command_tx, .. } => {
                    // Signal the driver thread to shut down cleanly, then
                    // kill the entire process group (setsid makes PGID == PID).
                    let _ = command_tx.send(ClaudeDriverCommand::Shutdown);
                    if let Some(pid) = agent.child_pid {
                        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    }
                }
                AgentBackend::CodexStdio { command_tx, .. } => {
                    let _ = command_tx.send(CodexDriverCommand::Shutdown);
                    if let Some(pid) = agent.child_pid {
                        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    }
                }
                AgentBackend::OpenCodeHttp { command_tx, .. } => {
                    let _ = command_tx.send(OpenCodeDriverCommand::Shutdown);
                    if let Some(pid) = agent.child_pid {
                        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    }
                }
            }
        }
    }
}

fn send_input(state: &Arc<Mutex<SupervisorState>>, agent_id: u32, bytes: &[u8]) {
    if let Ok(state) = state.lock() {
        if let Some(agent) = state.agents.get(&agent_id) {
            match &agent.backend {
                AgentBackend::Pty { writer, .. } => {
                    if let Ok(mut writer) = writer.lock() {
                        let _ = writer.write_all(bytes);
                        let _ = writer.flush();
                    }
                }
                AgentBackend::ClaudeWs { .. } => {
                    eprintln!("[claude-ws] raw input not supported, use SendPrompt");
                }
                AgentBackend::CodexStdio { .. } => {
                    eprintln!("[codex] raw input not supported, use SendPrompt");
                }
                AgentBackend::OpenCodeHttp { .. } => {
                    eprintln!("[opencode] raw input not supported, use SendPrompt");
                }
            }
        }
    }
}

fn resize_agent(state: &Arc<Mutex<SupervisorState>>, agent_id: u32, rows: u16, cols: u16) {
    if let Ok(state) = state.lock() {
        if let Some(agent) = state.agents.get(&agent_id) {
            match &agent.backend {
                AgentBackend::Pty { master, .. } => {
                    if let Ok(master) = master.lock() {
                        let _ = master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
                AgentBackend::ClaudeWs { .. } => {
                    // No-op: WS driver doesn't have a terminal to resize
                }
                AgentBackend::CodexStdio { .. } => {
                    // No-op: Codex driver doesn't have a terminal to resize
                }
                AgentBackend::OpenCodeHttp { .. } => {
                    // No-op: OpenCode HTTP driver doesn't have a terminal to resize
                }
            }
        }
    }
}

fn set_output_paused(state: &Arc<Mutex<SupervisorState>>, agent_id: u32, paused: bool) {
    if let Ok(mut state) = state.lock() {
        if let Some(agent) = state.agents.get_mut(&agent_id) {
            agent.output_paused = paused;
        }
    }
}

/// Route a SendPrompt to the appropriate driver (Claude WS or Codex).
fn dispatch_send_prompt(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    prompt: String,
) {
    if let Ok(s) = state.lock() {
        if let Some(agent) = s.agents.get(&agent_id) {
            match &agent.backend {
                AgentBackend::ClaudeWs { command_tx, .. } => {
                    if command_tx.send(ClaudeDriverCommand::SendPrompt { prompt }).is_err() {
                        eprintln!("[driver] agent {agent_id}: command channel closed");
                    }
                }
                AgentBackend::CodexStdio { command_tx, .. } => {
                    if command_tx.send(CodexDriverCommand::SendPrompt { prompt }).is_err() {
                        eprintln!("[driver] agent {agent_id}: command channel closed");
                    }
                }
                AgentBackend::OpenCodeHttp { command_tx, .. } => {
                    if command_tx.send(OpenCodeDriverCommand::SendPrompt { prompt }).is_err() {
                        eprintln!("[driver] agent {agent_id}: command channel closed");
                    }
                }
                AgentBackend::Pty { .. } => {
                    eprintln!("[driver] agent {agent_id}: SendPrompt not supported for PTY agents");
                }
            }
        }
    }
}

/// Route an Interrupt to the appropriate driver.
fn dispatch_interrupt(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
) {
    if let Ok(s) = state.lock() {
        if let Some(agent) = s.agents.get(&agent_id) {
            match &agent.backend {
                AgentBackend::ClaudeWs { command_tx, .. } => {
                    let _ = command_tx.send(ClaudeDriverCommand::Interrupt);
                }
                AgentBackend::CodexStdio { command_tx, .. } => {
                    let _ = command_tx.send(CodexDriverCommand::Interrupt);
                }
                AgentBackend::OpenCodeHttp { command_tx, .. } => {
                    let _ = command_tx.send(OpenCodeDriverCommand::Interrupt);
                }
                AgentBackend::Pty { .. } => {}
            }
        }
    }
}

/// Route a ToolApprovalResponse to the appropriate driver.
fn dispatch_tool_approval(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    request_id: String,
    approved: bool,
    updated_input: Option<Value>,
) {
    if let Ok(s) = state.lock() {
        if let Some(agent) = s.agents.get(&agent_id) {
            match &agent.backend {
                AgentBackend::ClaudeWs { command_tx, .. } => {
                    let _ = command_tx.send(ClaudeDriverCommand::ToolApprovalResponse {
                        request_id,
                        approved,
                        updated_input,
                    });
                }
                AgentBackend::CodexStdio { command_tx, .. } => {
                    // Codex uses JSON Value for request_id
                    let req_id: Value = serde_json::from_str(&request_id)
                        .unwrap_or(Value::String(request_id));
                    let _ = command_tx.send(CodexDriverCommand::ToolApprovalResponse {
                        request_id: req_id,
                        approved,
                    });
                }
                AgentBackend::OpenCodeHttp { command_tx, .. } => {
                    let _ = command_tx.send(OpenCodeDriverCommand::ToolApprovalResponse {
                        request_id,
                        approved,
                    });
                }
                AgentBackend::Pty { .. } => {}
            }
        }
    }
}

/// Route a QuestionResponse to the appropriate driver.
fn dispatch_question_response(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    request_id: String,
    answers: Vec<Vec<String>>,
    rejected: bool,
) {
    if let Ok(s) = state.lock() {
        if let Some(agent) = s.agents.get(&agent_id) {
            match &agent.backend {
                AgentBackend::OpenCodeHttp { command_tx, .. } => {
                    let _ = command_tx.send(OpenCodeDriverCommand::QuestionResponse {
                        request_id,
                        answers,
                        rejected,
                    });
                }
                AgentBackend::ClaudeWs { .. } | AgentBackend::CodexStdio { .. } | AgentBackend::Pty { .. } => {}
            }
        }
    }
}

fn snapshot(state: &Arc<Mutex<SupervisorState>>) -> Vec<AgentInfo> {
    match state.lock() {
        Ok(state) => state.snapshot_agents(),
        Err(poisoned) => poisoned.into_inner().snapshot_agents(),
    }
}

fn set_ws_config(state: &Arc<Mutex<SupervisorState>>, enabled: bool, password: String) {
    let was_running = {
        let mut s = state.lock().unwrap();
        let was = s.ws_listener_running;
        s.ws_enabled = enabled;
        s.ws_password = password.clone();
        was
    };

    // Persist to config file
    let mut config = crate::config::load_config();
    config.ws_enabled = enabled;
    config.ws_password = password;
    config.ws_port = state.lock().unwrap().ws_port;
    if let Err(e) = crate::config::save_config(&config) {
        eprintln!("Failed to save config: {e}");
    }

    // Start or stop the listener based on new config
    if enabled && !was_running {
        start_ws_listener(state);
    } else if !enabled && was_running {
        stop_ws_listener(state);
    }

    // Broadcast updated status to all clients
    broadcast_ws_status(state);
}

pub(crate) fn broadcast_ws_status(state: &Arc<Mutex<SupervisorState>>) {
    let (enabled, peers) = {
        let s = state.lock().unwrap();
        (s.ws_enabled, s.ws_peers.clone())
    };
    broadcast(state, &ServerMessage::WsStatus {
        enabled,
        connected_peers: peers,
    });
}

/// Start the WebSocket listener thread.
fn start_ws_listener(state: &Arc<Mutex<SupervisorState>>) {
    let shutdown = {
        let mut s = state.lock().unwrap();
        if s.ws_listener_running {
            eprintln!("[ws] listener already running");
            return;
        }
        // Reset shutdown flag for new listener
        s.ws_shutdown = Arc::new(AtomicBool::new(false));
        s.ws_listener_running = true;
        s.ws_shutdown.clone()
    };

    let state_ws = state.clone();
    thread::spawn(move || {
        if let Err(e) = super::ws::run_ws_listener(state_ws.clone(), shutdown) {
            eprintln!("[ws] listener error: {e}");
        }
        // Mark as not running when thread exits
        if let Ok(mut s) = state_ws.lock() {
            s.ws_listener_running = false;
        }
    });
    eprintln!("[ws] listener started");
}

/// Stop the WebSocket listener thread.
fn stop_ws_listener(state: &Arc<Mutex<SupervisorState>>) {
    let port = {
        let mut s = state.lock().unwrap();
        if !s.ws_listener_running {
            eprintln!("[ws] listener not running");
            return;
        }
        s.ws_shutdown.store(true, Ordering::SeqCst);
        // Clear connected peers since we're shutting down
        s.ws_peers.clear();
        s.ws_port
    };

    // Connect to the listener to wake it from accept() so it can see the shutdown flag
    // This is a common pattern for gracefully stopping a blocking listener
    if let Ok(stream) = std::net::TcpStream::connect(format!("127.0.0.1:{}", port)) {
        drop(stream); // Just connect and drop to wake the accept loop
    }

    eprintln!("[ws] listener stop requested");
}

/// Add a WebSocket peer to the tracked list.
pub(crate) fn add_ws_peer(state: &Arc<Mutex<SupervisorState>>, peer: String) {
    {
        let mut s = state.lock().unwrap();
        if !s.ws_peers.contains(&peer) {
            s.ws_peers.push(peer);
        }
    }
    broadcast_ws_status(state);
}

/// Remove a WebSocket peer from the tracked list.
pub(crate) fn remove_ws_peer(state: &Arc<Mutex<SupervisorState>>, peer: &str) {
    {
        let mut s = state.lock().unwrap();
        s.ws_peers.retain(|p| p != peer);
    }
    broadcast_ws_status(state);
}

/// Resume a previously exited agent from persistence.
fn resume_agent(state: &Arc<Mutex<SupervisorState>>, agent_id: u32) -> io::Result<()> {
    // Load agent from persistence
    let persisted = {
        let s = state.lock().unwrap();
        let store = s.store.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "No persistence store")
        })?;
        store.get(agent_id).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, e.to_string())
        })?
    };

    let persisted = persisted.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("Agent {} not found in database", agent_id))
    })?;

    // Remove old runtime entry if still present (exited agents linger)
    {
        let mut s = state.lock().unwrap();
        s.agents.remove(&agent_id);
    }

    // Remove old entry from database (will be re-persisted with new spawn)
    {
        let s = state.lock().unwrap();
        if let Some(ref store) = s.store {
            let _ = store.delete(agent_id);
        }
    }

    // For driver agents with a session_id, resume with history
    let has_session = persisted.session_id.is_some() || persisted.thread_id.is_some();
    if has_session && (persisted.agent_type == AgentType::ClaudeCode || persisted.agent_type == AgentType::Codex) {
        let output_tail = persisted.output_tail.clone();
        // Reuse the same agent_id so clients don't need to re-learn the ID.
        // The old AgentRuntime was already removed above; the spawn functions
        // will INSERT OR REPLACE into the DB with the same primary key.
        let id = agent_id;
        let repo_name = Path::new(&persisted.repo_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let canonical_repo = std::fs::canonicalize(&persisted.repo_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| persisted.repo_path.clone());

        // Don't re-send the original prompt on resume — thread/resume
        // restores conversation history, and the user will type a new prompt.
        // Model is None on resume — agent keeps whatever model it was using.
        if persisted.agent_type == AgentType::ClaudeCode {
            spawn_claude_code_ws(
                state, id, persisted.parent_id, persisted.agent_type,
                persisted.repo_path, repo_name.clone(), &canonical_repo,
                None, persisted.session_id, None,
            )?;
        } else {
            spawn_codex_driver(
                state, id, persisted.parent_id, persisted.agent_type,
                persisted.repo_path, repo_name.clone(), &canonical_repo,
                None, persisted.thread_id, None,
            )?;
        }

        // Show a compact "where we left off" snippet before the resumed stream.
        let (last_user, last_agent) =
            session_catalog::recent_messages_from_md_log(id, persisted.agent_type, &repo_name);
        let mut context = Vec::new();
        if let Some(user) = last_user {
            context.extend_from_slice(format!("{}\r\n", user).as_bytes());
        }
        if let Some(agent) = last_agent {
            context.extend_from_slice(format!("{}\r\n", agent).as_bytes());
        }
        if !context.is_empty() {
            context.extend_from_slice(b"[prior to resuming]\r\n\r\n");
            append_output(state, id, &context);
            broadcast(state, &ServerMessage::AgentOutput {
                agent_id: id,
                bytes: context,
            });
        }

        // Replay saved output tail so the UI shows previous conversation
        if let Some(tail) = output_tail {
            if !tail.is_empty() {
                append_output(state, id, &tail);
                broadcast(state, &ServerMessage::AgentOutput {
                    agent_id: id,
                    bytes: tail,
                });
            }
        }

        return Ok(());
    }

    // For agents without a session, spawn fresh
    spawn_agent(
        state,
        persisted.agent_type,
        persisted.repo_path,
        persisted.prompt,
        persisted.parent_id,
        None,
    )
}
