pub mod client;
pub mod framing;
pub mod proto;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::ipc::client::IpcClient;

pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("OPENSWARM_SOCKET") {
        return PathBuf::from(path);
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        Path::new(&dir).join("openswarm.sock")
    } else {
        Path::new("/tmp").join("openswarm.sock")
    }
}

/// Binary mtime as a build fingerprint. Both the supervisor and UI compute
/// this from `current_exe()` at startup — a mismatch means the binary was
/// rebuilt and the supervisor is stale.
pub fn build_id() -> u64 {
    std::env::current_exe()
        .ok()
        .and_then(|p| fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn pid_path(socket: &Path) -> PathBuf {
    socket.with_extension("pid")
}

fn kill_stale_supervisor(socket: &Path) {
    let pf = pid_path(socket);
    if let Ok(contents) = fs::read_to_string(&pf) {
        if let Ok(pid) = contents.trim().parse::<i32>() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            // Give the old process a moment to release the socket
            thread::sleep(Duration::from_millis(200));
        }
    }
    let _ = fs::remove_file(socket);
    let _ = fs::remove_file(&pf);
}

pub fn connect_or_spawn() -> io::Result<IpcClient> {
    let socket = socket_path();
    let expected = build_id();

    if let Ok(client) = IpcClient::connect(&socket) {
        if client.build_id() == expected {
            return Ok(client);
        }
        eprintln!("Supervisor build mismatch — restarting");
        drop(client);
        kill_stale_supervisor(&socket);
    }

    if std::env::var("OPENSWARM_NO_AUTOSTART").ok().as_deref() == Some("1") {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Supervisor not running and autostart disabled",
        ));
    }

    let exe = std::env::current_exe()?;
    let _ = Command::new(exe).arg("--supervisor").spawn();

    for _ in 0..20 {
        if let Ok(client) = IpcClient::connect(&socket) {
            return Ok(client);
        }
        thread::sleep(Duration::from_millis(100));
    }

    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "Failed to connect to supervisor",
    ))
}
