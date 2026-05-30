use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;

use crate::agent::types::AgentType;

pub(crate) struct SessionArtifact {
    started_at: DateTime<Utc>,
    agent_type: AgentType,
    repo_name: String,
    repo_path: String,
    started_in: String,
    md_path: PathBuf,
    transcript_path: PathBuf,
    sidecar_path: PathBuf,
    symlink_path: PathBuf,
}

impl SessionArtifact {
    pub(crate) fn create(
        agent_type: AgentType,
        repo_name: &str,
        repo_path: &str,
        started_in: &str,
        unique_hint: &str,
    ) -> io::Result<Self> {
        let started_at = Utc::now();
        let repo_slug = sanitize_slug(repo_name);
        let dir = session_store_root()
            .join(&repo_slug)
            .join(started_at.format("%Y-%m-%d").to_string());
        fs::create_dir_all(&dir)?;

        let stem = format!(
            "{}-{}-{}",
            started_at.format("%Y%m%d%H%M"),
            agent_type.label(),
            sanitize_slug(unique_hint)
        );

        let md_path = dir.join(format!("{stem}.md"));
        let transcript_path = dir.join(format!("{stem}.jsonl"));
        let sidecar_path = dir.join(format!("{stem}.json"));

        let symlink_dir = digtwin_logs_root().join(&repo_slug);
        fs::create_dir_all(&symlink_dir)?;
        let symlink_path = symlink_dir.join(format!(
            "{}-{}.md",
            started_at.format("%m%d%H%M"),
            agent_type.label()
        ));

        Ok(Self {
            started_at,
            agent_type,
            repo_name: repo_name.to_string(),
            repo_path: repo_path.to_string(),
            started_in: started_in.to_string(),
            md_path,
            transcript_path,
            sidecar_path,
            symlink_path,
        })
    }

    pub(crate) fn md_path(&self) -> &Path {
        &self.md_path
    }

    pub(crate) fn transcript_path(&self) -> &Path {
        &self.transcript_path
    }

    pub(crate) fn sidecar_path(&self) -> &Path {
        &self.sidecar_path
    }

    pub(crate) fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    pub(crate) fn repo_path(&self) -> &str {
        &self.repo_path
    }

    pub(crate) fn started_in(&self) -> &str {
        &self.started_in
    }

    pub(crate) fn write_sidecar(
        &self,
        session_handle: &str,
        ended_at: Option<DateTime<Utc>>,
    ) -> io::Result<()> {
        if session_handle.trim().is_empty() {
            return Ok(());
        }

        self.ensure_symlink()?;

        let value = json!({
            "schema_version": 1,
            "agent_type": self.agent_type.label(),
            "session_id": session_handle,
            "resume_command": resume_command(self.agent_type, session_handle),
            "repo_path": self.repo_path,
            "repo_name": self.repo_name,
            "started_in": self.started_in,
            "started_at": self.started_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "ended_at": ended_at.map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true)),
            "md_path": self.md_path.to_string_lossy(),
            "transcript_path": self.transcript_path.to_string_lossy(),
            "symlink_path": self.symlink_path.to_string_lossy(),
            "mobile_safe": false
        });

        if let Some(parent) = self.sidecar_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.sidecar_path, serde_json::to_vec_pretty(&value)?)?;
        Ok(())
    }

    fn ensure_symlink(&self) -> io::Result<()> {
        if let Some(parent) = self.symlink_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if self.symlink_path.exists() || self.symlink_path.symlink_metadata().is_ok() {
            fs::remove_file(&self.symlink_path)?;
        }
        unix_fs::symlink(&self.md_path, &self.symlink_path)?;
        Ok(())
    }
}

fn session_store_root() -> PathBuf {
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(data)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share")
    } else {
        PathBuf::from("/tmp")
    }
    .join("OpenSwarm")
    .join("sessions")
    .join("by-repo")
}

fn digtwin_logs_root() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("repos").join("digtwin").join("logs")
    } else {
        PathBuf::from("/tmp").join("digtwin-logs")
    }
}

fn sanitize_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn resume_command(agent_type: AgentType, session_handle: &str) -> String {
    match agent_type {
        AgentType::ClaudeCode => format!("claude --resume {session_handle}"),
        AgentType::Codex => format!("codex resume {session_handle}"),
        AgentType::OpenCode => format!("opencode resume {session_handle}"),
    }
}
