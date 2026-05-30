//! OpenCode HTTP/SSE driver (scaffold).
//!
//! Spawns `opencode serve`, creates a session, subscribes to `/event` via SSE,
//! and sends prompts/interrupts/permission replies over HTTP.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::blocking::Client as BlockingClient;
use serde_json::{json, Value};

use crate::agent::status::AgentStatus;
use crate::ipc::proto::{AgentEventType, ServerMessage};

use super::server::{append_output, broadcast, emit_event, set_status, SupervisorState};

pub(crate) enum OpenCodeDriverCommand {
    SendPrompt { prompt: String },
    ToolApprovalResponse { request_id: String, approved: bool },
    QuestionResponse {
        request_id: String,
        answers: Vec<Vec<String>>,
        rejected: bool,
    },
    Interrupt,
    Shutdown,
}

pub(crate) fn spawn_opencode(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    repo_path: &str,
    prompt: Option<String>,
    model: Option<String>,
) -> io::Result<(Arc<Mutex<Child>>, mpsc::Sender<OpenCodeDriverCommand>, u32)> {
    let port = alloc_local_port()?;
    let mut cmd = Command::new("opencode");
    cmd.arg("serve")
        .arg("--hostname")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string());
    let mut child = unsafe {
        cmd.current_dir(repo_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .pre_exec(|| {
                libc::setsid();
                Ok(())
            })
            .spawn()?
    };

    let child_pid = child.id();
    eprintln!("[opencode] agent {agent_id}: spawned serve pid={child_pid} port={port}");

    let stdout = child.stdout.take().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stderr"))?;

    let child = Arc::new(Mutex::new(child));
    let (command_tx, command_rx) = mpsc::channel::<OpenCodeDriverCommand>();

    let state_clone = state.clone();
    let child_clone = child.clone();
    let repo_path = repo_path.to_string();
    thread::spawn(move || {
        if let Err(e) = run_driver(
            state_clone,
            child_clone,
            agent_id,
            port,
            repo_path,
            stdout,
            stderr,
            command_rx,
            prompt,
            model,
        ) {
            eprintln!("[opencode] agent {agent_id}: driver error: {e}");
        }
    });

    Ok((child, command_tx, child_pid))
}

#[allow(clippy::too_many_arguments)]
fn run_driver(
    state: Arc<Mutex<SupervisorState>>,
    child: Arc<Mutex<Child>>,
    agent_id: u32,
    port: u16,
    _repo_path: String,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    command_rx: mpsc::Receiver<OpenCodeDriverCommand>,
    initial_prompt: Option<String>,
    model: Option<String>,
) -> io::Result<()> {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => eprintln!("[opencode-serve] {line}"),
                Err(_) => break,
            }
        }
    });
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(line) => eprintln!("[opencode-stderr] {line}"),
                Err(_) => break,
            }
        }
    });

    let base_url = format!("http://127.0.0.1:{port}");
    let http = build_http_client()?;
    wait_for_health(&http, &base_url)?;
    let session_id = create_session(&http, &base_url)?;

    {
        let mut st = state.lock().unwrap();
        if let Some(agent) = st.agents.get_mut(&agent_id) {
            agent.session_id = Some(session_id.clone());
        }
        if let Some(ref store) = st.store {
            let _ = store.update_session_id(agent_id, &session_id);
        }
    }

    emit_output(
        &state,
        agent_id,
        format!(
            "\r\nOpenCode Session {}\r\nModel: {}\r\n\r\n",
            session_id,
            model.clone().unwrap_or_else(|| crate::config::OPENCODE_DEFAULT_MODEL.to_string())
        )
        .as_bytes(),
    );
    emit_event(
        &state,
        agent_id,
        AgentEventType::SessionInit {
            model: model.clone().unwrap_or_else(|| crate::config::OPENCODE_DEFAULT_MODEL.to_string()),
            session_id: session_id.clone(),
        },
    );

    let (sse_done_tx, sse_done_rx) = mpsc::channel::<()>();
    start_sse_stream(state.clone(), agent_id, base_url.clone(), session_id.clone(), sse_done_tx)?;

    if let Some(p) = initial_prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        let _ = send_prompt_http(&http, &base_url, &session_id, p, model.as_deref());
        emit_output(&state, agent_id, format!("> {}\r\n", p).as_bytes());
    }

    loop {
        match command_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(OpenCodeDriverCommand::SendPrompt { prompt }) => {
                if let Err(e) = send_prompt_http(&http, &base_url, &session_id, &prompt, model.as_deref()) {
                    eprintln!("[opencode] agent {agent_id}: send prompt failed: {e}");
                    emit_event(&state, agent_id, AgentEventType::Error { message: e.to_string() });
                } else {
                    emit_output(&state, agent_id, format!("> {}\r\n", prompt).as_bytes());
                }
            }
            Ok(OpenCodeDriverCommand::Interrupt) => {
                let _ = post_json(&http, &format!("{base_url}/session/{session_id}/abort"), &json!({}));
            }
            Ok(OpenCodeDriverCommand::ToolApprovalResponse { request_id, approved }) => {
                let reply = if approved { "once" } else { "reject" };
                let _ = post_json(
                    &http,
                    &format!("{base_url}/permission/{request_id}/reply"),
                    &json!({ "reply": reply }),
                );
            }
            Ok(OpenCodeDriverCommand::QuestionResponse {
                request_id,
                answers,
                rejected,
            }) => {
                if rejected {
                    let _ = post_json(&http, &format!("{base_url}/question/{request_id}/reject"), &json!({}));
                } else {
                    let _ = post_json(
                        &http,
                        &format!("{base_url}/question/{request_id}/reply"),
                        &json!({ "answers": answers }),
                    );
                }
            }
            Ok(OpenCodeDriverCommand::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if sse_done_rx.try_recv().is_ok() {
            // SSE stream ended (server exited or connection broke)
            if child.lock().unwrap().try_wait()?.is_some() {
                break;
            }
        }
        if child.lock().unwrap().try_wait()?.is_some() {
            break;
        }
    }

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
            let _ = store.mark_exited(agent_id, &output_tail);
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
    Ok(())
}

fn alloc_local_port() -> io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn build_http_client() -> io::Result<BlockingClient> {
    BlockingClient::builder()
        .timeout(Duration::from_secs(20))
        .no_proxy()
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("reqwest client build failed: {e}")))
}

fn wait_for_health(http: &BlockingClient, base_url: &str) -> io::Result<()> {
    let url = format!("{base_url}/global/health");
    let start = std::time::Instant::now();
    loop {
        if get_json(http, &url).is_ok() {
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(15) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "opencode health check timed out"));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn create_session(http: &BlockingClient, base_url: &str) -> io::Result<String> {
    let v = post_json(http, &format!("{base_url}/session"), &json!({}))?;
    v.get("id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("missing session id in response: {v}")))
}

fn send_prompt_http(
    http: &BlockingClient,
    base_url: &str,
    session_id: &str,
    prompt: &str,
    model: Option<&str>,
) -> io::Result<()> {
    let mut body = json!({
        "parts": [
            { "type": "text", "text": prompt }
        ]
    });

    if let Some((provider_id, model_id)) = model.and_then(split_model_override) {
        body["model"] = json!({
            "providerID": provider_id,
            "modelID": model_id,
        });
    }

    // Use async prompt endpoint so the driver thread doesn't block waiting for
    // the full assistant response body; streaming output comes from SSE.
    let _ = post_json(http, &format!("{base_url}/session/{session_id}/prompt_async"), &body)?;
    Ok(())
}

fn split_model_override(model: &str) -> Option<(&str, &str)> {
    let (provider, model_id) = model.split_once('/')?;
    if provider.is_empty() || model_id.is_empty() {
        return None;
    }
    Some((provider, model_id))
}

fn start_sse_stream(
    state: Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    base_url: String,
    session_id: String,
    done_tx: mpsc::Sender<()>,
) -> io::Result<()> {

    thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[opencode] agent {agent_id}: tokio runtime init failed: {e}");
                let _ = done_tx.send(());
                return;
            }
        };

        rt.block_on(async move {
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .no_proxy()
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[opencode] agent {agent_id}: reqwest async client build failed: {e}");
                    let _ = done_tx.send(());
                    return;
                }
            };

            let response = match client.get(format!("{base_url}/event")).send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[opencode] agent {agent_id}: SSE connect failed: {e}");
                    let _ = done_tx.send(());
                    return;
                }
            };

            let mut stream = response.bytes_stream().eventsource();
            let mut part_text_seen: HashMap<String, String> = HashMap::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(evt) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&evt.data) {
                            handle_sse_event(&state, agent_id, &session_id, &v, &mut part_text_seen);
                        }
                    }
                    Err(e) => {
                        eprintln!("[opencode] agent {agent_id}: SSE stream error: {e}");
                        break;
                    }
                }
            }
            let _ = done_tx.send(());
        });
    });

    Ok(())
}

fn handle_sse_event(
    state: &Arc<Mutex<SupervisorState>>,
    agent_id: u32,
    session_id: &str,
    event: &Value,
    part_text_seen: &mut HashMap<String, String>,
) {
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let props = event.get("properties").unwrap_or(&Value::Null);

    match event_type {
        "server.connected" => {}
        "session.status" => {
            let sid = props.get("sessionID").and_then(|v| v.as_str()).unwrap_or("");
            if sid != session_id {
                return;
            }
            let status_ty = props
                .get("status")
                .and_then(|s| s.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let status = match status_ty {
                "idle" => AgentStatus::Idle,
                "busy" | "running" => AgentStatus::Running,
                _ => return,
            };
            set_status(state, agent_id, status);
            broadcast(
                state,
                &ServerMessage::AgentStatus {
                    agent_id,
                    status,
                },
            );
        }
        "message.part.updated" => {
            let part = match props.get("part") {
                Some(p) => p,
                None => return,
            };
            let sid = part.get("sessionID").and_then(|v| v.as_str()).unwrap_or("");
            if sid != session_id {
                return;
            }
            let part_id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if part_type == "reasoning" {
                emit_event(state, agent_id, AgentEventType::Thinking);
                return;
            }
            if part_type != "text" || part_id.is_empty() || text.is_empty() {
                return;
            }
            let prev = part_text_seen.get(part_id).cloned().unwrap_or_default();
            let delta = if text.starts_with(&prev) {
                &text[prev.len()..]
            } else {
                text
            };
            if !delta.is_empty() {
                emit_output(state, agent_id, delta.as_bytes());
            }
            part_text_seen.insert(part_id.to_string(), text.to_string());
        }
        "permission.asked" => {
            let sid = props.get("sessionID").and_then(|v| v.as_str()).unwrap_or("");
            if sid != session_id {
                return;
            }
            let request_id = props.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if request_id.is_empty() {
                return;
            }
            let tool_name = props
                .get("permission")
                .and_then(|v| v.as_str())
                .unwrap_or("permission")
                .to_string();
            emit_event(state, agent_id, AgentEventType::WaitingForInput);
            broadcast(
                state,
                &ServerMessage::ToolApprovalRequest {
                    agent_id,
                    request_id,
                    tool_name,
                    tool_input: props.clone(),
                    description: None,
                },
            );
        }
        "question.asked" => {
            let sid = props.get("sessionID").and_then(|v| v.as_str()).unwrap_or("");
            if sid != session_id {
                return;
            }
            emit_event(state, agent_id, AgentEventType::WaitingForInput);
            let request_id = props.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let questions = props.get("questions").cloned().unwrap_or(Value::Array(Vec::new()));
            broadcast(
                state,
                &ServerMessage::QuestionRequest {
                    agent_id,
                    request_id,
                    questions,
                },
            );
        }
        "session.error" => {
            let sid = props.get("sessionID").and_then(|v| v.as_str()).unwrap_or("");
            if !sid.is_empty() && sid != session_id {
                return;
            }
            let msg = extract_session_error_message(props).unwrap_or_else(|| "session error".to_string());
            emit_event(state, agent_id, AgentEventType::Error { message: msg.to_string() });
            emit_output(state, agent_id, format!("\r\nERROR: {msg}\r\n").as_bytes());
        }
        _ => {}
    }
}

fn extract_session_error_message(props: &Value) -> Option<String> {
    let err = props.get("error")?;
    if let Some(msg) = err.get("data").and_then(|d| d.get("message")).and_then(|v| v.as_str()) {
        let s = msg.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(msg) = err.get("message").and_then(|v| v.as_str()) {
        let s = msg.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(name) = err.get("name").and_then(|v| v.as_str()) {
        let s = name.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    Some(err.to_string())
}

fn emit_output(state: &Arc<Mutex<SupervisorState>>, agent_id: u32, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    append_output(state, agent_id, bytes);
    let paused = state
        .lock()
        .map(|s| s.agents.get(&agent_id).map_or(false, |a| a.output_paused))
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

fn get_json(http: &BlockingClient, url: &str) -> io::Result<Value> {
    let resp = http
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("GET {url} failed: {e}")))?;
    resp.json::<Value>()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("invalid json from {url}: {e}")))
}

fn post_json(http: &BlockingClient, url: &str, body: &Value) -> io::Result<Value> {
    let resp = http
        .post(url)
        .json(body)
        .send()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("POST {url} failed: {e}")))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("POST {url} returned error: {e}")))?;
    let bytes = resp
        .bytes()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("POST {url} read body failed: {e}")))?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("invalid json POST {url}: {e}")))
}
