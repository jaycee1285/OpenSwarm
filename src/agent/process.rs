use std::cell::Cell;
use std::rc::Rc;

use glib;
use gtk4::gio;
use vte4::prelude::*;

use crate::agent::types::AgentType;

/// Build argv for spawning an agent CLI.
pub fn build_argv(agent_type: AgentType, prompt: &Option<String>) -> Vec<String> {
    let cmd = agent_type.command().to_string();
    match prompt {
        Some(p) if !p.is_empty() => vec![cmd, p.clone()],
        _ => vec![cmd],
    }
}

/// Spawn an agent process in a VTE terminal.
/// Stores the child PID in `pid_cell` when the spawn callback fires.
pub fn spawn_agent(
    terminal: &vte4::Terminal,
    working_directory: &str,
    argv: &[String],
    pid_cell: Rc<Cell<Option<i32>>>,
) {
    let argv_strs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        Some(working_directory),
        &argv_strs,
        &[],
        glib::SpawnFlags::SEARCH_PATH,
        || {},
        -1,
        None::<&gio::Cancellable>,
        move |result| match result {
            Ok(pid) => {
                pid_cell.set(Some(pid.0));
            }
            Err(e) => {
                eprintln!("Failed to spawn agent: {e}");
            }
        },
    );
}

/// Send SIGTERM to an agent process. Returns true if signal was sent.
pub fn kill_agent(pid: i32) -> bool {
    unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
}
