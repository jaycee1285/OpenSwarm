//! SQLite-based persistence for agent history.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Result as SqlResult};

use crate::agent::status::AgentStatus;
use crate::agent::types::AgentType;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER,
    agent_type TEXT NOT NULL,
    repo_path TEXT NOT NULL,
    prompt TEXT,
    spawn_time TEXT NOT NULL,
    exit_time TEXT,
    last_status TEXT NOT NULL,
    output_tail BLOB,
    session_id TEXT,
    thread_id TEXT
);

CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(last_status);
"#;

/// Persisted agent record.
#[derive(Debug, Clone)]
pub struct PersistedAgent {
    pub id: u32,
    pub parent_id: Option<u32>,
    pub agent_type: AgentType,
    pub repo_path: String,
    pub prompt: Option<String>,
    pub spawn_time: DateTime<Utc>,
    pub exit_time: Option<DateTime<Utc>>,
    pub last_status: AgentStatus,
    pub output_tail: Option<Vec<u8>>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
}

/// Database path for agent storage.
fn db_path() -> PathBuf {
    if let Some(data_dir) = dirs::data_dir() {
        data_dir.join("openswarm/agents.db")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/openswarm/agents.db")
    } else {
        PathBuf::from("openswarm-agents.db")
    }
}

/// Agent persistence store.
pub struct AgentStore {
    conn: Connection,
}

impl AgentStore {
    /// Open or create the agent database.
    pub fn open() -> SqlResult<Self> {
        let path = db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(SCHEMA)?;
        // Migrations: add columns that may be missing in older DBs
        let _ = conn.execute("ALTER TABLE agents ADD COLUMN session_id TEXT", []);
        let _ = conn.execute("ALTER TABLE agents ADD COLUMN thread_id TEXT", []);
        Ok(Self { conn })
    }

    /// Persist a new agent.
    pub fn persist(&self, agent: &PersistedAgent) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO agents (id, parent_id, agent_type, repo_path, prompt, spawn_time, exit_time, last_status, output_tail, session_id, thread_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                agent.id,
                agent.parent_id,
                agent_type_to_str(agent.agent_type),
                agent.repo_path,
                agent.prompt,
                agent.spawn_time.to_rfc3339(),
                agent.exit_time.map(|t| t.to_rfc3339()),
                status_to_str(agent.last_status),
                agent.output_tail,
                agent.session_id,
                agent.thread_id,
            ],
        )?;
        Ok(())
    }

    /// Update agent status.
    pub fn update_status(&self, id: u32, status: AgentStatus) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE agents SET last_status = ?1 WHERE id = ?2",
            params![status_to_str(status), id],
        )?;
        Ok(())
    }

    /// Update the session_id for a Claude agent (set when CLI sends system/init).
    pub fn update_session_id(&self, id: u32, session_id: &str) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE agents SET session_id = ?1 WHERE id = ?2",
            params![session_id, id],
        )?;
        Ok(())
    }

    /// Update the thread_id for a Codex agent (set when thread/start responds).
    pub fn update_thread_id(&self, id: u32, thread_id: &str) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE agents SET thread_id = ?1 WHERE id = ?2",
            params![thread_id, id],
        )?;
        Ok(())
    }

    /// Mark agent as exited with output tail.
    pub fn mark_exited(&self, id: u32, output_tail: &[u8]) -> SqlResult<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE agents SET last_status = 'Exited', exit_time = ?1, output_tail = ?2 WHERE id = ?3",
            params![now, output_tail, id],
        )?;
        Ok(())
    }

    /// Load all persisted agents.
    pub fn load_all(&self) -> SqlResult<Vec<PersistedAgent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, parent_id, agent_type, repo_path, prompt, spawn_time, exit_time, last_status, output_tail, session_id, thread_id FROM agents ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PersistedAgent {
                id: row.get::<_, i64>(0)? as u32,
                parent_id: row.get::<_, Option<i64>>(1)?.map(|v| v as u32),
                agent_type: str_to_agent_type(&row.get::<_, String>(2)?),
                repo_path: row.get(3)?,
                prompt: row.get(4)?,
                spawn_time: parse_datetime(&row.get::<_, String>(5)?),
                exit_time: row.get::<_, Option<String>>(6)?.map(|s| parse_datetime(&s)),
                last_status: str_to_status(&row.get::<_, String>(7)?),
                output_tail: row.get(8)?,
                session_id: row.get(9)?,
                thread_id: row.get(10)?,
            })
        })?;
        rows.collect()
    }

    /// Load most recent agents by spawn_time descending.
    pub fn load_recent(&self, limit: usize) -> SqlResult<Vec<PersistedAgent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, parent_id, agent_type, repo_path, prompt, spawn_time, exit_time, last_status, output_tail, session_id, thread_id
             FROM agents
             ORDER BY spawn_time DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(PersistedAgent {
                id: row.get::<_, i64>(0)? as u32,
                parent_id: row.get::<_, Option<i64>>(1)?.map(|v| v as u32),
                agent_type: str_to_agent_type(&row.get::<_, String>(2)?),
                repo_path: row.get(3)?,
                prompt: row.get(4)?,
                spawn_time: parse_datetime(&row.get::<_, String>(5)?),
                exit_time: row.get::<_, Option<String>>(6)?.map(|s| parse_datetime(&s)),
                last_status: str_to_status(&row.get::<_, String>(7)?),
                output_tail: row.get(8)?,
                session_id: row.get(9)?,
                thread_id: row.get(10)?,
            })
        })?;
        rows.collect()
    }

    /// Get a single agent by ID.
    pub fn get(&self, id: u32) -> SqlResult<Option<PersistedAgent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, parent_id, agent_type, repo_path, prompt, spawn_time, exit_time, last_status, output_tail, session_id, thread_id FROM agents WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |row| {
            Ok(PersistedAgent {
                id: row.get::<_, i64>(0)? as u32,
                parent_id: row.get::<_, Option<i64>>(1)?.map(|v| v as u32),
                agent_type: str_to_agent_type(&row.get::<_, String>(2)?),
                repo_path: row.get(3)?,
                prompt: row.get(4)?,
                spawn_time: parse_datetime(&row.get::<_, String>(5)?),
                exit_time: row.get::<_, Option<String>>(6)?.map(|s| parse_datetime(&s)),
                last_status: str_to_status(&row.get::<_, String>(7)?),
                output_tail: row.get(8)?,
                session_id: row.get(9)?,
                thread_id: row.get(10)?,
            })
        })?;
        rows.next().transpose()
    }

    /// Get the maximum agent ID in the database.
    pub fn max_id(&self) -> SqlResult<u32> {
        let result: Option<i64> = self.conn.query_row(
            "SELECT MAX(id) FROM agents",
            [],
            |row| row.get(0),
        )?;
        Ok(result.unwrap_or(0) as u32)
    }

    /// Delete an agent by ID.
    pub fn delete(&self, id: u32) -> SqlResult<()> {
        self.conn.execute("DELETE FROM agents WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn agent_type_to_str(t: AgentType) -> &'static str {
    match t {
        AgentType::ClaudeCode => "ClaudeCode",
        AgentType::Codex => "Codex",
        AgentType::OpenCode => "OpenCode",
    }
}

fn str_to_agent_type(s: &str) -> AgentType {
    match s {
        "Codex" => AgentType::Codex,
        "OpenCode" => AgentType::OpenCode,
        _ => AgentType::ClaudeCode,
    }
}

fn status_to_str(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Running => "Running",
        AgentStatus::Idle => "Idle",
        AgentStatus::Exited => "Exited",
    }
}

fn str_to_status(s: &str) -> AgentStatus {
    match s {
        "Idle" => AgentStatus::Idle,
        "Exited" => AgentStatus::Exited,
        _ => AgentStatus::Running,
    }
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_persist_and_load() {
        // This would need a temp directory override for testing
        // Skipping for now as it requires more setup
    }
}
