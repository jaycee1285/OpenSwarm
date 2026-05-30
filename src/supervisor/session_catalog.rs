use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::agent::types::AgentType;
use crate::ipc::proto::{RecentSessionInfo, RepoInfo};

use super::server::SupervisorState;

#[derive(Debug, Deserialize)]
struct ExportedSessionSidecar {
    agent_type: String,
    session_id: String,
    resume_command: String,
    repo_path: String,
    repo_name: String,
    started_at: String,
    md_path: String,
}

fn repos_root() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home).join("repos"));
    }
    dirs::home_dir().map(|home| home.join("repos"))
}

pub(crate) fn repo_inventory_snapshot() -> Vec<RepoInfo> {
    let Some(root) = repos_root() else {
        return Vec::new();
    };

    let mut repos = match fs::read_dir(&root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let file_type = entry.file_type().ok()?;
                if !file_type.is_dir() {
                    return None;
                }
                let repo_name = entry.file_name().to_string_lossy().to_string();
                let repo_path = path
                    .canonicalize()
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                Some(RepoInfo { repo_name, repo_path })
            })
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };

    repos.sort_by(|a, b| a.repo_name.to_lowercase().cmp(&b.repo_name.to_lowercase()));
    repos
}

fn exported_sessions_root() -> PathBuf {
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

fn collect_json_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();

    fn walk(current: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(current) else {
            return;
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                walk(&path, out);
            } else if file_type.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json")
            {
                out.push(path);
            }
        }
    }

    walk(root, &mut out);
    out
}

fn parse_agent_type(name: &str) -> Option<AgentType> {
    match name {
        "claude" | "claudecode" | "claude_code" => Some(AgentType::ClaudeCode),
        "codex" => Some(AgentType::Codex),
        "opencode" => Some(AgentType::OpenCode),
        _ => None,
    }
}

fn recent_messages_from_md_path(md_path: &str) -> (Option<String>, Option<String>) {
    let text = match fs::read_to_string(md_path) {
        Ok(t) => t,
        Err(_) => return (None, None),
    };

    let mut current_role: Option<&str> = None;
    let mut section = String::new();
    let mut last_user: Option<String> = None;
    let mut last_agent: Option<String> = None;

    let mut flush = |role: Option<&str>, body: &str| {
        let normalized = condense_for_preview(body);
        if normalized.is_empty() {
            return;
        }
        match role {
            Some("user") => last_user = Some(normalized),
            Some("assistant") => last_agent = Some(normalized),
            _ => {}
        }
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            flush(current_role, &section);
            section.clear();
            if rest.starts_with("Turn ") && rest.contains("User") {
                current_role = Some("user");
            } else {
                current_role = None;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("### ") {
            flush(current_role, &section);
            section.clear();
            if rest.starts_with("Assistant") {
                current_role = Some("assistant");
            } else {
                current_role = None;
            }
            continue;
        }
        if current_role.is_some() {
            section.push_str(line);
            section.push('\n');
        }
    }
    flush(current_role, &section);

    (last_user, last_agent)
}

fn exported_session_sidecars() -> Vec<ExportedSessionSidecar> {
    let root = exported_sessions_root();
    let mut sessions = collect_json_files(&root)
        .into_iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .filter_map(|text| serde_json::from_str::<ExportedSessionSidecar>(&text).ok())
        .collect::<Vec<_>>();

    sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    sessions
}

fn parse_rfc3339_utc(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn match_resumable_agent<'a>(
    sidecar: &ExportedSessionSidecar,
    agent_type: AgentType,
    persisted: &'a [crate::persistence::db::PersistedAgent],
) -> Option<&'a crate::persistence::db::PersistedAgent> {
    if agent_type == AgentType::ClaudeCode {
        if let Some(row) = persisted.iter().find(|row| {
            row.agent_type == agent_type
                && row.repo_path == sidecar.repo_path
                && row.session_id.as_deref() == Some(sidecar.session_id.as_str())
        }) {
            return Some(row);
        }
    }

    let Some(started_at) = parse_rfc3339_utc(&sidecar.started_at) else {
        return None;
    };

    persisted
        .iter()
        .filter(|row| row.agent_type == agent_type && row.repo_path == sidecar.repo_path)
        .filter_map(|row| {
            let delta = (row.spawn_time.timestamp() - started_at.timestamp()).abs();
            if delta <= 600 {
                Some((delta, row))
            } else {
                None
            }
        })
        .min_by_key(|(delta, _)| *delta)
        .map(|(_, row)| row)
}

fn can_resume_exported_session(agent_type: AgentType, repo_path: &str) -> bool {
    matches!(agent_type, AgentType::ClaudeCode | AgentType::Codex) && Path::new(repo_path).exists()
}

pub(crate) fn recent_sessions_snapshot(
    state: &Arc<Mutex<SupervisorState>>,
    limit: usize,
) -> Vec<RecentSessionInfo> {
    let persisted = {
        let s = state.lock().unwrap();
        let Some(ref store) = s.store else {
            return Vec::new();
        };
        match store.load_all() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        }
    };

    let mut grouped_counts: HashMap<String, usize> = HashMap::new();

    exported_session_sidecars()
        .into_iter()
        .filter_map(|sidecar| {
            let agent_type = parse_agent_type(&sidecar.agent_type)?;
            let repo_path = sidecar.repo_path.clone();
            let group_key = format!("{}::{}", sidecar.repo_path, agent_type.label());
            let count = grouped_counts.entry(group_key).or_insert(0);
            if *count >= limit {
                return None;
            }
            *count += 1;

            let matched = match_resumable_agent(&sidecar, agent_type, &persisted);
            let (last_user_message, last_agent_message) = recent_messages_from_md_path(&sidecar.md_path);
            let date_mmdd = parse_rfc3339_utc(&sidecar.started_at)?
                .with_timezone(&chrono::Local)
                .format("%m/%d")
                .to_string();

            Some(RecentSessionInfo {
                id: matched.map(|row| row.id).unwrap_or(0),
                repo_name: sidecar.repo_name,
                repo_path,
                agent_type,
                can_resume: matched.is_some()
                    || can_resume_exported_session(agent_type, &sidecar.repo_path),
                session_handle: Some(sidecar.session_id),
                date_mmdd,
                resume_hint: Some(sidecar.resume_command),
                last_user_message,
                last_agent_message,
            })
        })
        .collect()
}

fn condense_for_preview(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 140;
    let mut end = collapsed.len();
    let mut chars = collapsed.char_indices();
    for _ in 0..MAX {
        match chars.next() {
            Some((idx, _)) => end = idx,
            None => return collapsed,
        }
    }
    if chars.next().is_some() {
        format!("{}...", &collapsed[..end])
    } else {
        collapsed
    }
}

fn logs_dir() -> PathBuf {
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(data)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share")
    } else {
        PathBuf::from("/tmp")
    }
    .join("OpenSwarm")
    .join("logs")
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

pub(crate) fn recent_messages_from_md_log(
    agent_id: u32,
    agent_type: AgentType,
    repo_name: &str,
) -> (Option<String>, Option<String>) {
    let file_name = format!(
        "agent-{}-{}-{}.md",
        agent_id,
        sanitize_slug(repo_name),
        agent_type.label()
    );
    let path = logs_dir().join(file_name);
    let path_string = path.to_string_lossy().to_string();
    recent_messages_from_md_path(&path_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, tempdir};

    #[test]
    fn condense_preview_short_text_unchanged() {
        assert_eq!(condense_for_preview("hello world"), "hello world");
    }

    #[test]
    fn condense_preview_long_text_truncates() {
        let long = "a".repeat(220);
        let condensed = condense_for_preview(&long);
        assert!(condensed.ends_with("..."));
        assert!(condensed.len() < long.len());
    }

    #[test]
    fn sanitize_slug_normalizes_and_falls_back() {
        assert_eq!(sanitize_slug("Repo Name/With Weird*Chars"), "repo-name-with-weird-chars");
        assert_eq!(sanitize_slug("%%%"), "unknown");
    }

    #[test]
    fn parse_agent_type_accepts_aliases() {
        assert_eq!(parse_agent_type("claude"), Some(AgentType::ClaudeCode));
        assert_eq!(parse_agent_type("claude_code"), Some(AgentType::ClaudeCode));
        assert_eq!(parse_agent_type("codex"), Some(AgentType::Codex));
        assert_eq!(parse_agent_type("opencode"), Some(AgentType::OpenCode));
        assert_eq!(parse_agent_type("unknown"), None);
    }

    #[test]
    fn collect_json_files_walks_nested_directories() {
        let dir = tempdir().expect("tempdir");
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).expect("create nested");

        let top_json = dir.path().join("top.json");
        let nested_json = nested.join("nested.json");
        let txt = nested.join("ignore.txt");

        fs::write(&top_json, "{}").expect("write top json");
        fs::write(&nested_json, "{}").expect("write nested json");
        fs::write(&txt, "no").expect("write txt");

        let mut files = collect_json_files(dir.path());
        files.sort();

        assert_eq!(files.len(), 2);
        assert!(files.contains(&top_json));
        assert!(files.contains(&nested_json));
    }

    #[test]
    fn recent_messages_from_md_path_extracts_last_user_and_assistant() {
        let mut file = NamedTempFile::new().expect("temp file");
        let body = r#"
# Session
## Turn 1 User
first user question
### Assistant
first assistant answer
## Turn 2 User
final user question
### Assistant
final assistant answer
"#;
        file.write_all(body.as_bytes()).expect("write md");
        let path = file.path().to_string_lossy().to_string();

        let (user, assistant) = recent_messages_from_md_path(&path);
        assert_eq!(user.as_deref(), Some("final user question"));
        assert_eq!(assistant.as_deref(), Some("final assistant answer"));
    }
}
