use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::agent::types::AgentType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MdLogMode {
    /// Structured protocol logging (user + assistant messages only).
    StructuredAssistantOnly,
    RenderedOutputFallback,
}

pub(crate) struct ConversationMdLog {
    mode: MdLogMode,
    path: PathBuf,
    mirror_path: Option<PathBuf>,
    writer: BufWriter<fs::File>,
    mirror_writer: Option<BufWriter<fs::File>>,
    ansi_carry: Vec<u8>,
    wrote_fallback_section_header: bool,
}

impl ConversationMdLog {
    pub(crate) fn open(
        agent_id: u32,
        agent_type: AgentType,
        repo_name: &str,
        repo_path: &str,
        mode: MdLogMode,
    ) -> io::Result<Self> {
        let dir = logs_dir();
        fs::create_dir_all(&dir)?;

        let file_name = format!(
            "agent-{}-{}-{}.md",
            agent_id,
            sanitize_slug(repo_name),
            agent_type.label()
        );
        let path = dir.join(&file_name);

        Self::open_internal(path, Some(file_name), agent_id, agent_type, repo_path, mode)
    }

    pub(crate) fn open_at(
        path: PathBuf,
        agent_id: u32,
        agent_type: AgentType,
        repo_path: &str,
        mode: MdLogMode,
    ) -> io::Result<Self> {
        Self::open_internal(path, None, agent_id, agent_type, repo_path, mode)
    }

    fn open_internal(
        path: PathBuf,
        mirror_name: Option<String>,
        agent_id: u32,
        agent_type: AgentType,
        repo_path: &str,
        mode: MdLogMode,
    ) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file_exists = path.exists();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut writer = BufWriter::new(file);
        let mirror_path = mirror_name
            .as_deref()
            .and_then(mirror_log_path);
        let mut mirror_writer = mirror_path
            .as_ref()
            .and_then(open_append_writer);

        if !file_exists || fs::metadata(&path).map(|m| m.len()).unwrap_or(0) == 0 {
            let ts = Utc::now();
            writer.write_all(
                render_header(
                    agent_type,
                    repo_path,
                    repo_path,
                    ts,
                    None,
                    None,
                )
                .as_bytes(),
            )?;
            writer.flush()?;
            if let Some(ref mut mw) = mirror_writer {
                mw.write_all(
                    render_header(
                        agent_type,
                        repo_path,
                        repo_path,
                        ts,
                        None,
                        None,
                    )
                    .as_bytes(),
                )?;
                mw.flush()?;
            }
        }

        eprintln!("[md-log] agent {agent_id}: {}", path.display());

        Ok(Self {
            mode,
            path,
            mirror_path,
            writer,
            mirror_writer,
            ansi_carry: Vec::new(),
            wrote_fallback_section_header: false,
        })
    }

    pub(crate) fn mode(&self) -> MdLogMode {
        self.mode
    }

    pub(crate) fn finalize_header(
        &mut self,
        agent_type: AgentType,
        repo_path: &str,
        started_in: &str,
        started_at: DateTime<Utc>,
        session_handle: &str,
        model: Option<&str>,
    ) {
        let resume = match agent_type {
            AgentType::ClaudeCode => format!("claude --resume {session_handle}"),
            AgentType::Codex => format!("codex resume {session_handle}"),
            AgentType::OpenCode => format!("opencode resume {session_handle}"),
        };
        let new_header = render_header(
            agent_type,
            repo_path,
            started_in,
            started_at,
            Some((&resume, session_handle)),
            model,
        );
        let main_path = self.path.clone();
        self.rewrite_header(&main_path, &new_header);
        if let Some(path) = self.mirror_path.clone() {
            self.rewrite_header(&path, &new_header);
        }
    }

    pub(crate) fn append_assistant_message(&mut self, text: &str) {
        if self.mode != MdLogMode::StructuredAssistantOnly {
            return;
        }
        self.append_message_section("Assistant", text);
    }

    pub(crate) fn append_user_prompt(&mut self, text: &str) {
        self.append_message_section("User", text);
    }

    fn append_message_section(&mut self, role: &str, text: &str) {
        let normalized = normalize_text(text);
        if normalized.trim().is_empty() {
            return;
        }

        let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let _ = writeln!(self.writer, "## {} ({})", role, ts);
        let _ = writeln!(self.writer);
        let _ = writeln!(self.writer, "{}", normalized.trim_end());
        let _ = writeln!(self.writer);
        let _ = self.writer.flush();

        if let Some(ref mut w) = self.mirror_writer {
            let _ = writeln!(w, "## {} ({})", role, ts);
            let _ = writeln!(w);
            let _ = writeln!(w, "{}", normalized.trim_end());
            let _ = writeln!(w);
            let _ = w.flush();
        }
    }

    pub(crate) fn append_rendered_output_chunk(&mut self, bytes: &[u8]) {
        if self.mode != MdLogMode::RenderedOutputFallback || bytes.is_empty() {
            return;
        }
        let text = self.strip_ansi_streaming(bytes);
        if text.is_empty() {
            return;
        }
        let normalized = normalize_text(&text);
        if normalized.trim().is_empty() {
            return;
        }

        if !self.wrote_fallback_section_header {
            let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            let _ = writeln!(self.writer, "## Visible Output (PTY Fallback) ({})", ts);
            let _ = writeln!(self.writer);
            if let Some(ref mut w) = self.mirror_writer {
                let _ = writeln!(w, "## Visible Output (PTY Fallback) ({})", ts);
                let _ = writeln!(w);
            }
            self.wrote_fallback_section_header = true;
        }

        let _ = write!(self.writer, "{}", normalized);
        if !normalized.ends_with('\n') {
            let _ = writeln!(self.writer);
        }
        let _ = self.writer.flush();
        if let Some(ref mut w) = self.mirror_writer {
            let _ = write!(w, "{}", normalized);
            if !normalized.ends_with('\n') {
                let _ = writeln!(w);
            }
            let _ = w.flush();
        }
    }

    fn strip_ansi_streaming(&mut self, bytes: &[u8]) -> String {
        let mut input = Vec::with_capacity(self.ansi_carry.len() + bytes.len());
        if !self.ansi_carry.is_empty() {
            input.extend_from_slice(&self.ansi_carry);
            self.ansi_carry.clear();
        }
        input.extend_from_slice(bytes);

        let mut out = Vec::with_capacity(input.len());
        let mut i = 0usize;
        while i < input.len() {
            if input[i] != 0x1b {
                if input[i] >= 0x20 || input[i] == b'\n' || input[i] == b'\r' || input[i] == b'\t'
                {
                    out.push(input[i]);
                }
                i += 1;
                continue;
            }

            let esc_start = i;
            i += 1;
            if i >= input.len() {
                self.ansi_carry.extend_from_slice(&input[esc_start..]);
                break;
            }

            match input[i] {
                b'[' => {
                    i += 1;
                    let param_start = i;
                    while i < input.len() && (input[i] < 0x40 || input[i] > 0x7e) {
                        i += 1;
                    }
                    if i >= input.len() {
                        self.ansi_carry.extend_from_slice(&input[esc_start..]);
                        break;
                    }
                    let final_byte = input[i];
                    let params = &input[param_start..i];
                    i += 1;

                    if final_byte == b'C' {
                        let n = parse_csi_number(params).unwrap_or(1);
                        for _ in 0..n.min(80) {
                            out.push(b' ');
                        }
                    } else if final_byte == b'B' {
                        out.push(b'\n');
                    }
                }
                b']' => {
                    i += 1;
                    let mut terminated = false;
                    while i < input.len() {
                        if input[i] == 0x07 {
                            i += 1;
                            terminated = true;
                            break;
                        }
                        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' {
                            i += 2;
                            terminated = true;
                            break;
                        }
                        i += 1;
                    }
                    if !terminated {
                        self.ansi_carry.extend_from_slice(&input[esc_start..]);
                        break;
                    }
                }
                b'(' | b')' | b'>' | b'<' => {
                    i += 1;
                    if i > input.len() {
                        self.ansi_carry.extend_from_slice(&input[esc_start..]);
                        break;
                    }
                }
                _ => {
                    i += 1;
                }
            }
        }

        String::from_utf8_lossy(&out).to_string()
    }

    fn rewrite_header(&mut self, path: &PathBuf, header: &str) {
        let _ = self.writer.flush();
        if let Some(ref mut writer) = self.mirror_writer {
            let _ = writer.flush();
        }
        let existing = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return,
        };
        let body = if let Some(idx) = existing.find("\n---\n\n") {
            &existing[idx + 6..]
        } else {
            &existing
        };
        if fs::write(path, format!("{header}{body}")).is_err() {
            return;
        }
        if path == &self.path {
            if let Some(writer) = open_append_writer(path) {
                self.writer = writer;
            }
        } else if let Some(ref mirror_path) = self.mirror_path {
            if path == mirror_path {
                self.mirror_writer = open_append_writer(path);
            }
        }
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

fn mirror_logs_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let base = PathBuf::from(home).join("syncthing");
    let preferred = base.join("OSLogs");
    if preferred.exists() {
        return Some(preferred);
    }
    let legacy = base.join("OSlogs");
    if legacy.exists() {
        return Some(legacy);
    }
    Some(preferred)
}

fn mirror_log_path(file_name: &str) -> Option<PathBuf> {
    let dir = mirror_logs_dir()?;
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }
    Some(dir.join(file_name))
}

fn open_append_writer(path: &PathBuf) -> Option<BufWriter<fs::File>> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    Some(BufWriter::new(file))
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

fn normalize_text(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn parse_csi_number(params: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(params).ok()?;
    let first = s.split(';').next()?;
    if first.is_empty() {
        return None;
    }
    first.parse::<usize>().ok()
}

fn render_header(
    agent_type: AgentType,
    repo_path: &str,
    started_in: &str,
    started_at: DateTime<Utc>,
    resume: Option<(&str, &str)>,
    model: Option<&str>,
) -> String {
    let title = match agent_type {
        AgentType::ClaudeCode => "Claude Session",
        AgentType::Codex => "Codex Session",
        AgentType::OpenCode => "OpenCode Session",
    };

    let mut out = String::new();
    out.push_str(&format!("# {title}\n\n"));
    out.push_str(&format!("- **Repository**: `{repo_path}`\n"));
    if let Some((resume_cmd, session_handle)) = resume {
        out.push_str(&format!("- **Session ID**: `{session_handle}`\n"));
        out.push_str(&format!("- **Resume**: `{resume_cmd}`\n"));
    } else {
        out.push_str("- **Session ID**: `pending`\n");
        out.push_str("- **Resume**: `pending`\n");
    }
    out.push_str(&format!("- **Started In**: `{started_in}`\n"));
    if let Some(model) = model {
        out.push_str(&format!("- **Model**: `{model}`\n"));
    }
    out.push_str(&format!(
        "- **Started**: {}\n\n---\n\n",
        started_at.to_rfc3339_opts(SecondsFormat::Millis, true)
    ));
    out
}
