use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tungstenite::protocol::Message;

use crate::ipc::proto::{ClientMessage, ServerMessage};
use super::server::{
    ClientWriter, SupervisorState,
    dispatch_client_message, register_and_welcome, remove_client,
    add_ws_peer, remove_ws_peer,
};

const DEFAULT_WS_PORT: u16 = 9384;
const WS_READ_TIMEOUT_MS: u64 = 50;
const WS_PING_INTERVAL_SECS: u64 = 15;
const WS_IDLE_TIMEOUT_SECS: u64 = 60;

/// WebSocket client writer — sends JSON text frames (no length prefix).
struct WsClientWriter {
    tx: mpsc::Sender<String>,
    next_seq: u64,
    last_sent_seq: Arc<AtomicU64>,
}

impl ClientWriter for WsClientWriter {
    fn send_message(&mut self, msg: &ServerMessage) -> io::Result<()> {
        let mut v = serde_json::to_value(msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        if let serde_json::Value::Object(ref mut map) = v {
            map.insert("seq".to_string(), serde_json::Value::from(seq));
        }
        self.last_sent_seq.store(seq, Ordering::Relaxed);
        let json = serde_json::to_string(&v)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.tx
            .send(json)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ws writer disconnected"))
    }
}

#[derive(Deserialize)]
struct AuthMessage {
    auth: String,
}

pub fn run_ws_listener(
    state: Arc<Mutex<SupervisorState>>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    // Get port from env, then config, then default
    let port: u16 = std::env::var("OPENSWARM_WS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| {
            state.lock().map(|s| s.ws_port).unwrap_or(DEFAULT_WS_PORT)
        });

    // Get token from env first, then config
    let token = std::env::var("OPENSWARM_WS_TOKEN")
        .ok()
        .or_else(|| {
            let s = state.lock().ok()?;
            if s.ws_password.is_empty() {
                None
            } else {
                Some(s.ws_password.clone())
            }
        })
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "No WebSocket password configured")
        })?;

    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))?;
    // Set non-blocking so we can check shutdown flag periodically
    listener.set_nonblocking(true)?;
    eprintln!("[ws] listener on 0.0.0.0:{port}");

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, addr)) => {
                // Set the client stream back to blocking for normal I/O
                let _ = stream.set_nonblocking(false);
                let peer = addr.to_string();
                eprintln!("[ws] TCP connection from {peer}");
                let state = state.clone();
                let token = token.clone();
                thread::spawn(move || {
                    match handle_ws_client(stream, state, &token) {
                        Ok(()) => eprintln!("[ws] client {peer} disconnected cleanly"),
                        Err(e) => eprintln!("[ws] client {peer} error: {e}"),
                    }
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No connection pending, sleep briefly and check shutdown again
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[ws] TCP accept error: {e}");
            }
        }
    }

    eprintln!("[ws] listener shutting down");
    Ok(())
}

fn handle_ws_client(
    stream: std::net::TcpStream,
    state: Arc<Mutex<SupervisorState>>,
    expected_token: &str,
) -> io::Result<()> {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "unknown".into());

    eprintln!("[ws] {peer}: starting WebSocket handshake");
    let mut ws = tungstenite::accept(stream)
        .map_err(|e| {
            eprintln!("[ws] {peer}: handshake failed: {e}");
            io::Error::new(io::ErrorKind::ConnectionRefused, e)
        })?;
    eprintln!("[ws] {peer}: handshake complete, waiting for auth message");

    // Auth handshake: first message must be {"auth": "token"}
    let auth_msg = ws
        .read()
        .map_err(|e| {
            eprintln!("[ws] {peer}: failed reading auth message: {e}");
            io::Error::new(io::ErrorKind::ConnectionAborted, e)
        })?;

    let auth_text = match &auth_msg {
        Message::Text(t) => t.clone(),
        other => {
            eprintln!("[ws] {peer}: expected text auth message, got {:?}", other);
            let _ = ws.close(None);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "expected text auth message",
            ));
        }
    };

    let auth: AuthMessage = serde_json::from_str(&auth_text)
        .map_err(|e| {
            eprintln!("[ws] {peer}: invalid auth JSON: {e}");
            io::Error::new(io::ErrorKind::PermissionDenied, "invalid auth JSON")
        })?;

    if auth.auth != expected_token {
        eprintln!("[ws] {peer}: auth token rejected");
        let _ = ws.close(None);
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid auth token",
        ));
    }
    eprintln!("[ws] {peer}: auth OK");

    // Track this peer
    let peer_ip = peer.split(':').next().unwrap_or(&peer).to_string();
    add_ws_peer(&state, peer_ip.clone());

    // Use a short read timeout so the read loop releases the shared socket
    // mutex regularly, allowing broadcast writes to make progress.
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(WS_READ_TIMEOUT_MS)))?;

    // Shared WebSocket between the read loop and a per-client writer thread.
    // Broadcast callers enqueue messages and return quickly instead of writing directly.
    let ws = Arc::new(Mutex::new(ws));
    let (write_tx, write_rx) = mpsc::channel::<String>();
    let last_sent_seq = Arc::new(AtomicU64::new(0));
    let last_acked_seq = Arc::new(AtomicU64::new(0));
    let ws_writer = ws.clone();
    let peer_writer = peer.clone();
    let writer_thread = thread::spawn(move || {
        while let Ok(json) = write_rx.recv() {
            let send_result = {
                let mut ws = ws_writer.lock().unwrap();
                ws.send(Message::Text(json))
            };
            if let Err(e) = send_result {
                eprintln!("[ws] {peer_writer}: writer send error: {e}");
                break;
            }
        }
        eprintln!("[ws] {peer_writer}: writer thread exiting");
    });

    let writer: Arc<Mutex<dyn ClientWriter>> =
        Arc::new(Mutex::new(WsClientWriter {
            tx: write_tx,
            next_seq: 1,
            last_sent_seq: last_sent_seq.clone(),
        }));

    eprintln!("[ws] {peer}: registering client and sending welcome");
    register_and_welcome(&state, &writer);
    eprintln!("[ws] {peer}: welcome sent, entering message loop");

    let mut last_rx = Instant::now();
    let mut last_ping = Instant::now();

    // Message loop
    loop {
        let msg = {
            let mut ws = ws.lock().unwrap();
            ws.read()
        };

        match msg {
            Ok(Message::Text(text)) => {
                last_rx = Instant::now();
                match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::Ack { last_seq }) => {
                        last_acked_seq.store(last_seq, Ordering::Relaxed);
                    }
                    Ok(client_msg) => dispatch_client_message(client_msg, &state),
                    Err(e) => {
                        eprintln!("[ws] {peer}: skipping unrecognised message: {e}");
                        continue;
                    }
                }
            }
            Ok(Message::Close(frame)) => {
                eprintln!("[ws] {peer}: received close frame: {frame:?}");
                break;
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                last_rx = Instant::now();
                continue;
            }
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                // Read timeout is expected; it prevents the read loop from monopolizing
                // the shared WebSocket mutex needed by broadcast writes.
                let now = Instant::now();
                if now.duration_since(last_rx) >= Duration::from_secs(WS_IDLE_TIMEOUT_SECS) {
                    eprintln!(
                        "[ws] {peer}: idle timeout ({}s) without inbound traffic, closing client",
                        WS_IDLE_TIMEOUT_SECS
                    );
                    break;
                }
                if now.duration_since(last_ping) >= Duration::from_secs(WS_PING_INTERVAL_SECS) {
                    let sent = last_sent_seq.load(Ordering::Relaxed);
                    let acked = last_acked_seq.load(Ordering::Relaxed);
                    if sent > acked && (sent - acked) > 100 {
                        eprintln!(
                            "[ws] {peer}: ack lag (sent={}, acked={}, pending={})",
                            sent,
                            acked,
                            sent - acked
                        );
                    }
                    let ping_result = {
                        let mut ws = ws.lock().unwrap();
                        ws.send(Message::Ping(Vec::new()))
                    };
                    match ping_result {
                        Ok(()) => last_ping = now,
                        Err(e) => {
                            eprintln!("[ws] {peer}: ping send error: {e}");
                            break;
                        }
                    }
                }
                continue;
            }
            Err(e) => {
                eprintln!("[ws] {peer}: read error: {e}");
                break;
            }
            Ok(other) => {
                eprintln!("[ws] {peer}: ignoring non-text frame: {other:?}");
                continue;
            }
        }
    }

    remove_client(&state, &writer);
    drop(writer);
    remove_ws_peer(&state, &peer_ip);
    let sent = last_sent_seq.load(Ordering::Relaxed);
    let acked = last_acked_seq.load(Ordering::Relaxed);
    eprintln!(
        "[ws] {peer}: session delivery summary sent={} acked={} pending={}",
        sent,
        acked,
        sent.saturating_sub(acked)
    );
    let _ = writer_thread.join();
    Ok(())
}
