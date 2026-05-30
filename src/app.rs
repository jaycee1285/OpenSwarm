use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use gtk4 as gtk;

use crate::agent::status::AgentStatus;
use crate::agent::types::AgentType;
use crate::ipc::client::IpcClient;
use crate::ipc::proto::{AgentInfo, RecentSessionInfo, RepoInfo};
use crate::ui::agent_row;
use crate::ui::dashboard::DashboardState;
use crate::ui::terminal_input;
use vte4::prelude::*;

pub struct AgentEntry {
    pub id: u32,
    pub parent_id: Option<u32>,
    pub agent_type: AgentType,
    pub repo_path: PathBuf,
    pub repo_name: String,
    pub terminal: vte4::Terminal,
    pub status: Rc<Cell<AgentStatus>>,
    pub status_dot: gtk::Label,
    pub last_output: Rc<Cell<Instant>>,
    pub last_pty_cols: Cell<i64>,
    pub last_pty_rows: Cell<i64>,
    pub row: gtk::ListBoxRow,
    /// Line buffer for ClaudeCode WS agents (typed input before Enter).
    pub input_buffer: Rc<RefCell<String>>,
    /// Accumulated dashboard metrics from AgentEvent messages.
    pub dashboard: DashboardState,
}

pub struct AppState {
    pub agents: RefCell<Vec<AgentEntry>>,
    pub selected_id: Cell<Option<u32>>,
    pub stack: gtk::Stack,
    pub list_box: gtk::ListBox,
    pub ipc: Rc<IpcClient>,
    pub repo_inventory: RefCell<Vec<RepoInfo>>,
    pub recent_sessions: RefCell<Vec<RecentSessionInfo>>,
    pub usage_snapshot: RefCell<Option<UsageSnapshot>>,
}

#[derive(Clone, Debug)]
pub struct UsageSnapshot {
    pub raw_output: String,
    pub session_percent: Option<u32>,
    pub session_reset: Option<String>,
    pub week_all_percent: Option<u32>,
    pub week_all_reset: Option<String>,
    pub week_sonnet_percent: Option<u32>,
    pub week_sonnet_reset: Option<String>,
    pub session_messages: Option<u32>,
    pub session_limit: Option<u32>,
    pub daily_messages: Option<u32>,
    pub weekly_messages: Option<u32>,
    pub messages_used: Option<u32>,
    pub messages_limit: Option<u32>,
    pub plan_tier: Option<String>,
    pub codex_five_hour_percent: Option<u32>,
    pub codex_five_hour_reset: Option<String>,
    pub codex_weekly_percent: Option<u32>,
    pub codex_weekly_reset: Option<String>,
}

impl AppState {
    pub fn new(stack: gtk::Stack, list_box: gtk::ListBox, ipc: Rc<IpcClient>) -> Rc<Self> {
        Rc::new(Self {
            agents: RefCell::new(Vec::new()),
            selected_id: Cell::new(None),
            stack,
            list_box,
            ipc,
            repo_inventory: RefCell::new(Vec::new()),
            recent_sessions: RefCell::new(Vec::new()),
            usage_snapshot: RefCell::new(None),
        })
    }

    /// Add agent entry to state and UI. Call select_row separately after this.
    pub fn add_agent(&self, entry: AgentEntry) {
        self.stack
            .add_named(&entry.terminal, Some(&entry.id.to_string()));
        self.list_box.append(&entry.row);
        self.agents.borrow_mut().push(entry);
    }

    pub fn add_agent_from_info(&self, info: AgentInfo) {
        if self.agents.borrow().iter().any(|a| a.id == info.id) {
            return;
        }

        let status = Rc::new(Cell::new(info.status));
        let last_output = Rc::new(Cell::new(Instant::now()));
        let input_buffer = Rc::new(RefCell::new(String::new()));

        let terminal = vte4::Terminal::new();
        terminal.set_scroll_on_output(true);
        terminal.set_scroll_on_keystroke(true);
        terminal.set_scrollback_lines(10000);
        crate::config::apply_terminal_theme(&terminal);
        terminal_input::attach(
            &terminal,
            info.id,
            info.agent_type,
            input_buffer.clone(),
            self.ipc.clone(),
        );

        // Forward terminal-generated responses (e.g. cursor position reports)
        // back to the agent through IPC, since VTE has no PTY attached.
        // Skip for ClaudeCode and Codex — driver agents have no PTY to receive these.
        if info.agent_type != AgentType::ClaudeCode && info.agent_type != AgentType::Codex {
            let ipc = self.ipc.clone();
            let agent_id = info.id;
            terminal.connect_commit(move |_terminal, text, _size| {
                ipc.send(&crate::ipc::proto::ClientMessage::Input {
                    agent_id,
                    bytes: text.as_bytes().to_vec(),
                });
            });
        }

        let (row, dot) = agent_row::create(info.id, info.agent_type, &info.repo_name, status.clone());

        let entry = AgentEntry {
            id: info.id,
            parent_id: info.parent_id,
            agent_type: info.agent_type,
            repo_path: PathBuf::from(info.repo_path),
            repo_name: info.repo_name,
            terminal,
            status,
            status_dot: dot,
            last_output,
            last_pty_cols: Cell::new(0),
            last_pty_rows: Cell::new(0),
            row,
            input_buffer,
            dashboard: DashboardState::new(),
        };

        self.add_agent(entry);
    }

    pub fn select_next(&self) {
        let agents = self.agents.borrow();
        if agents.is_empty() {
            return;
        }
        let current = self
            .selected_id
            .get()
            .and_then(|id| agents.iter().position(|a| a.id == id))
            .unwrap_or(0);
        let next = (current + 1) % agents.len();
        let row = agents[next].row.clone();
        drop(agents);
        self.list_box.select_row(Some(&row));
    }

    pub fn select_prev(&self) {
        let agents = self.agents.borrow();
        if agents.is_empty() {
            return;
        }
        let current = self
            .selected_id
            .get()
            .and_then(|id| agents.iter().position(|a| a.id == id))
            .unwrap_or(0);
        let prev = if current == 0 {
            agents.len() - 1
        } else {
            current - 1
        };
        let row = agents[prev].row.clone();
        drop(agents);
        self.list_box.select_row(Some(&row));
    }

    pub fn select_by_index(&self, index: usize) {
        let agents = self.agents.borrow();
        if let Some(entry) = agents.get(index) {
            let row = entry.row.clone();
            drop(agents);
            self.list_box.select_row(Some(&row));
        }
    }

    pub fn kill_selected(&self) {
        let selected_id = self.selected_id.get();
        if let Some(id) = selected_id {
            self.ipc
                .send(&crate::ipc::proto::ClientMessage::KillAgent { agent_id: id });
        }
    }

    pub fn resume_selected(&self) {
        let selected_id = self.selected_id.get();
        if let Some(id) = selected_id {
            let agents = self.agents.borrow();
            if let Some(entry) = agents.iter().find(|a| a.id == id) {
                if entry.status.get() == AgentStatus::Exited {
                    self.ipc
                        .send(&crate::ipc::proto::ClientMessage::ResumeAgent { agent_id: id });
                }
            }
        }
    }

    pub fn remove_selected(&self) {
        self.kill_selected();

        if let Some(id) = self.selected_id.get() {
            let mut agents = self.agents.borrow_mut();
            if let Some(pos) = agents.iter().position(|a| a.id == id) {
                let entry = agents.remove(pos);
                self.list_box.remove(&entry.row);
                self.stack.remove(&entry.terminal);
            }
            drop(agents);

            let agents = self.agents.borrow();
            if let Some(entry) = agents.first() {
                let row = entry.row.clone();
                drop(agents);
                self.selected_id.set(None);
                self.list_box.select_row(Some(&row));
            } else {
                drop(agents);
                self.selected_id.set(None);
                self.stack.set_visible_child_name("empty");
            }
        }
    }
}
