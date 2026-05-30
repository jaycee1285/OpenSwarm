//! Usage polling — probes `claude /usage` via a ghost PTY for real
//! session/week percentages.  Falls back to reading local files when the
//! probe fails (e.g. `claude` not installed or times out).
//!
//! Probe flow:
//!   1. Spawn `claude --dangerously-skip-permissions` in a portable-pty PTY
//!   2. Wait for the interactive prompt
//!   3. Send `/usage\n`
//!   4. Read output, strip ANSI escapes, parse percentages + reset times
//!   5. Send Escape + kill process
//!
//! Fallback reads:
//!   - `~/.claude/.credentials.json` — subscription type, rate limit tier
//!   - `~/.claude/stats-cache.json`  — daily activity, token counts

use std::io::{self, BufRead, Read as _, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::supervisor::server::{broadcast, usage_status_message, SupervisorState, UsageInfo};

/// Guard to prevent concurrent probes (only one claude process at a time).
static PROBE_RUNNING: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// ANSI escape stripping
// ---------------------------------------------------------------------------

/// Strip ANSI escape sequences from raw PTY output, returning plain text.
/// Cursor-forward sequences (ESC[NC) are replaced with spaces so words
/// aren't concatenated.
fn strip_ansi(input: &[u8]) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            i += 1;
            if i >= input.len() {
                break;
            }
            match input[i] {
                // CSI: ESC [ <params> <final byte>
                b'[' => {
                    i += 1;
                    // Collect parameter bytes
                    let param_start = i;
                    while i < input.len() && (input[i] < 0x40 || input[i] > 0x7e) {
                        i += 1;
                    }
                    if i < input.len() {
                        let final_byte = input[i];
                        let params = &input[param_start..i];
                        i += 1;

                        // Cursor Forward (CUF): ESC[<n>C — replace with n spaces
                        if final_byte == b'C' {
                            let n = parse_csi_number(params).unwrap_or(1);
                            for _ in 0..n.min(80) {
                                out.push(b' ');
                            }
                        }
                        // Cursor Down + beginning of line: treat ESC[<n>B as newline
                        else if final_byte == b'B' {
                            out.push(b'\n');
                        }
                        // All other CSI sequences are dropped (colors, cursor positioning, etc.)
                    }
                }
                // OSC: ESC ] ... (terminated by BEL or ST)
                b']' => {
                    i += 1;
                    while i < input.len() {
                        if input[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                // Two-char sequences: ESC ( , ESC ) , ESC > , ESC < , etc.
                b'(' | b')' | b'>' | b'<' => {
                    i += 1; // skip the char after
                }
                // Single-char ESC sequences
                _ => {
                    i += 1;
                }
            }
        } else if input[i] < 0x20 && input[i] != b'\n' && input[i] != b'\r' && input[i] != b'\t' {
            // Skip other control characters
            i += 1;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Parse the numeric parameter from CSI bytes (e.g. "12" from ESC[12C).
fn parse_csi_number(params: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(params).ok()?;
    // CSI params can be semicolon-separated; take the first/only one
    let first = s.split(';').next()?;
    if first.is_empty() {
        return None;
    }
    first.parse::<usize>().ok()
}

// ---------------------------------------------------------------------------
// /usage output parser
// ---------------------------------------------------------------------------

/// Parsed result from the `/usage` panel.
#[derive(Default, Debug)]
struct UsageProbeResult {
    session_percent: Option<u32>,
    session_reset: Option<String>,
    week_all_percent: Option<u32>,
    week_all_reset: Option<String>,
    week_sonnet_percent: Option<u32>,
    week_sonnet_reset: Option<String>,
}

#[derive(Clone, Copy)]
enum Section {
    Session,
    WeekAll,
    WeekSonnet,
}

fn parse_usage_output(raw: &str) -> UsageProbeResult {
    let mut result = UsageProbeResult::default();
    let mut current_section: Option<Section> = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Detect section headers
        if trimmed.contains("Current session") {
            current_section = Some(Section::Session);
            continue;
        }
        if trimmed.contains("Current week (all models)") {
            current_section = Some(Section::WeekAll);
            continue;
        }
        if trimmed.contains("Current week (Sonnet only)") || trimmed.contains("Current week (sonnet only)") {
            current_section = Some(Section::WeekSonnet);
            continue;
        }

        // Parse "N% used"
        if let Some(section) = current_section {
            if trimmed.contains("% used") {
                if let Some(pct) = extract_percent(trimmed) {
                    match section {
                        Section::Session => result.session_percent = Some(pct),
                        Section::WeekAll => result.week_all_percent = Some(pct),
                        Section::WeekSonnet => result.week_sonnet_percent = Some(pct),
                    }
                }
                continue;
            }

            // Parse "Resets ..."
            if trimmed.starts_with("Resets ") {
                let reset_str = trimmed.to_string();
                match section {
                    Section::Session => result.session_reset = Some(reset_str),
                    Section::WeekAll => result.week_all_reset = Some(reset_str),
                    Section::WeekSonnet => result.week_sonnet_reset = Some(reset_str),
                }
                continue;
            }
        }
    }

    result
}

/// Extract percentage from a string like "██▌  5% used" or "40% used".
fn extract_percent(s: &str) -> Option<u32> {
    // Find "N% used" pattern
    let idx = s.find("% used")?;
    let before = &s[..idx];
    // Walk backwards to find the start of the number
    let num_str: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    num_str.parse::<u32>().ok()
}

// ---------------------------------------------------------------------------
// Ghost PTY probe
// ---------------------------------------------------------------------------

const PROBE_LOG: &str = "/tmp/openswarm-usage-probe.log";

/// Append a timestamped line to the probe log file.
fn probe_log(msg: &str) {
    use std::fs::OpenOptions;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(PROBE_LOG) {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

/// Dump raw bytes as hex + printable ASCII to the probe log.
fn probe_log_raw(label: &str, data: &[u8]) {
    use std::fs::OpenOptions;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(PROBE_LOG) {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        let _ = writeln!(f, "[{}] {} ({} bytes):", ts, label, data.len());
        // Hex dump, 64 bytes per line
        for chunk in data.chunks(64) {
            let hex: String = chunk.iter().map(|b| format!("{:02x} ", b)).collect();
            let ascii: String = chunk
                .iter()
                .map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' })
                .collect();
            let _ = writeln!(f, "  {} | {}", hex.trim_end(), ascii);
        }
    }
}

/// Spawn `claude --dangerously-skip-permissions`, send `/usage`, parse output.
fn probe_usage() -> io::Result<UsageProbeResult> {
    probe_log("=== probe_usage() starting ===");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let mut cmd = CommandBuilder::new("claude");
    cmd.arg("--dangerously-skip-permissions");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| {
            probe_log(&format!("spawn failed: {e}"));
            io::Error::new(io::ErrorKind::Other, e)
        })?;

    probe_log("claude spawned, setting up reader/writer");

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    // Read in a background thread to avoid blocking
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut accumulated = Vec::new();

    // Phase 1: Wait for prompt (up to 8 seconds)
    probe_log("phase 1: waiting for prompt (8s timeout)");
    let start = Instant::now();
    let prompt_timeout = Duration::from_secs(8);
    let mut prompt_ready = false;
    while start.elapsed() < prompt_timeout {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(data) => {
                accumulated.extend_from_slice(&data);
                let stripped = strip_ansi(&accumulated);
                // Look for the prompt indicator (❯) or the permissions warning
                if stripped.contains('\u{276F}') || stripped.contains("bypass permissions") {
                    prompt_ready = true;
                    probe_log(&format!("prompt detected after {:.1}s", start.elapsed().as_secs_f64()));
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }

    if !prompt_ready {
        probe_log("FAIL: prompt not detected within 8s");
        probe_log_raw("accumulated raw (phase 1)", &accumulated);
        let stripped = strip_ansi(&accumulated);
        probe_log(&format!("stripped (phase 1):\n{}", stripped));
        let _ = child.kill();
        drop(reader_handle);
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "claude prompt did not appear within 8s",
        ));
    }

    // Phase 2: Send /usage in two steps — text first, then Enter after picker appears
    thread::sleep(Duration::from_millis(500));
    probe_log("phase 2a: sending /usage text");
    let _ = writer.write_all(b"/usage");
    let _ = writer.flush();

    // Wait for the autocomplete picker to appear and settle
    thread::sleep(Duration::from_secs(2));

    // Drain any picker output so it doesn't pollute our usage parsing
    while let Ok(_) = rx.try_recv() {}
    accumulated.clear();

    probe_log("phase 2b: sending Enter");
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();

    // Phase 3: Read /usage output (up to 15 seconds, or until we see "% used" + "Resets")
    accumulated.clear();
    let usage_start = Instant::now();
    let usage_timeout = Duration::from_secs(15);
    let mut got_usage = false;
    probe_log("phase 3: reading /usage output (15s timeout)");
    while usage_start.elapsed() < usage_timeout {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(data) => {
                accumulated.extend_from_slice(&data);
                let stripped = strip_ansi(&accumulated);
                if stripped.contains("% used") && stripped.contains("Resets ") {
                    probe_log(&format!("usage markers found after {:.1}s, draining", usage_start.elapsed().as_secs_f64()));
                    // Wait a bit more for all sections to arrive
                    thread::sleep(Duration::from_millis(500));
                    // Drain remaining
                    while let Ok(more) = rx.recv_timeout(Duration::from_millis(200)) {
                        accumulated.extend_from_slice(&more);
                    }
                    got_usage = true;
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }

    // Phase 4: Send Escape and kill
    probe_log("phase 4: sending Escape + kill");
    let _ = writer.write_all(b"\x1b");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    drop(reader_handle);

    if !got_usage {
        probe_log("FAIL: no /usage output within 15s");
        probe_log_raw("accumulated raw (phase 3)", &accumulated);
        let stripped = strip_ansi(&accumulated);
        probe_log(&format!("stripped (phase 3):\n{}", stripped));
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "did not receive /usage output within 15s",
        ));
    }

    // Log everything for debugging
    probe_log_raw("raw /usage output", &accumulated);
    let stripped = strip_ansi(&accumulated);
    probe_log(&format!("stripped output ({} chars):\n{}", stripped.len(), &stripped));

    let result = parse_usage_output(&stripped);
    probe_log(&format!("parsed result: {:?}", result));

    if result.session_percent.is_none() && result.week_all_percent.is_none() {
        probe_log("FAIL: no percentages parsed from output");
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "failed to parse any percentages from /usage output",
        ));
    }

    probe_log(&format!("SUCCESS: session={}% week_all={}% week_sonnet={}%",
        result.session_percent.unwrap_or(0),
        result.week_all_percent.unwrap_or(0),
        result.week_sonnet_percent.unwrap_or(0),
    ));

    Ok(result)
}

// ---------------------------------------------------------------------------
// File-based fallback (existing logic)
// ---------------------------------------------------------------------------

fn tier_limit(tier: &str) -> Option<u32> {
    match tier {
        "default_claude_pro" => Some(45),
        "default_claude_max_5x" => Some(225),
        "default_claude_max_20x" => Some(900),
        _ => None,
    }
}

fn claude_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".claude")
}

fn read_plan_tier() -> Option<(String, String)> {
    let path = claude_dir().join(".credentials.json");
    let data = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;

    let oauth = v.get("claudeAiOauth")?;
    let sub_type = oauth
        .get("subscriptionType")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let tier = oauth
        .get("rateLimitTier")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    Some((sub_type, tier))
}

#[derive(Default)]
struct DailyStats {
    date: String,
    message_count: u64,
    session_count: u64,
    tool_call_count: u64,
}

#[derive(Default)]
struct ModelStats {
    total_cost_usd: f64,
    total_input_tokens: u64,
    total_output_tokens: u64,
}

#[derive(Default)]
struct RollingStats {
    today_messages: u32,
    week_messages: u32,
}

fn read_stats_cache() -> Option<(DailyStats, ModelStats, RollingStats)> {
    let path = claude_dir().join("stats-cache.json");
    let data = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;

    let daily_arr = v.get("dailyActivity")?.as_array()?;
    let daily = daily_arr.last()?;

    let ds = DailyStats {
        date: daily.get("date").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        message_count: daily.get("messageCount").and_then(|v| v.as_u64()).unwrap_or(0),
        session_count: daily.get("sessionCount").and_then(|v| v.as_u64()).unwrap_or(0),
        tool_call_count: daily.get("toolCallCount").and_then(|v| v.as_u64()).unwrap_or(0),
    };

    let mut week_messages: u32 = 0;
    for entry in daily_arr.iter().rev().take(7) {
        let m = entry.get("messageCount").and_then(|v| v.as_u64()).unwrap_or(0);
        week_messages = week_messages.saturating_add(m as u32);
    }
    let rolling = RollingStats {
        today_messages: ds.message_count as u32,
        week_messages,
    };

    let mut ms = ModelStats::default();
    if let Some(model_usage) = v.get("modelUsage").and_then(|v| v.as_object()) {
        for (_model, usage) in model_usage {
            ms.total_cost_usd += usage.get("costUSD").and_then(|v| v.as_f64()).unwrap_or(0.0);
            ms.total_input_tokens += usage.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
            ms.total_output_tokens += usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
        }
    }

    Some((ds, ms, rolling))
}

fn transcript_dir() -> PathBuf {
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(data)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share")
    } else {
        PathBuf::from("/tmp")
    }
    .join("openswarm")
    .join("transcripts")
}

fn count_session_messages(window_hours: i64) -> Option<u32> {
    let dir = transcript_dir();
    if !dir.exists() {
        return None;
    }

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(window_hours);
    let mut count: u32 = 0;

    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }

        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                let modified: chrono::DateTime<chrono::Utc> = modified.into();
                if modified < cutoff - chrono::Duration::hours(24) {
                    continue;
                }
            }
        }

        let file = std::fs::File::open(&path).ok()?;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().flatten() {
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let dir = v.get("dir").and_then(|v| v.as_str()).unwrap_or("");
            if dir != "out" {
                continue;
            }
            let ts = v.get("ts").and_then(|v| v.as_str()).unwrap_or("");
            let ts = match chrono::DateTime::parse_from_rfc3339(ts) {
                Ok(t) => t.with_timezone(&chrono::Utc),
                Err(_) => continue,
            };
            if ts < cutoff {
                continue;
            }
            let msg_type = v
                .get("msg")
                .and_then(|m| m.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if msg_type == "user" {
                count = count.saturating_add(1);
            }
        }
    }

    Some(count)
}

fn poll_from_files() -> io::Result<UsageInfo> {
    let (sub_type, tier) = read_plan_tier()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "credentials not found"))?;

    let limit = tier_limit(&tier);

    let (daily, model, rolling) = read_stats_cache()
        .unwrap_or_default();

    let session_messages = count_session_messages(5);

    let raw = format!(
        "Plan: {} ({})\nToday ({}): {} messages, {} sessions, {} tool calls\nAll-time cost: ${:.2} ({} in / {} out tokens)",
        sub_type, tier,
        daily.date, daily.message_count, daily.session_count, daily.tool_call_count,
        model.total_cost_usd, model.total_input_tokens, model.total_output_tokens,
    );

    Ok(UsageInfo {
        raw_output: raw,
        session_percent: None,
        session_reset: None,
        week_all_percent: None,
        week_all_reset: None,
        week_sonnet_percent: None,
        week_sonnet_reset: None,
        session_messages,
        session_limit: limit,
        daily_messages: Some(rolling.today_messages),
        weekly_messages: Some(rolling.week_messages),
        messages_used: None,
        messages_limit: limit,
        plan_tier: Some(format!("{} ({})", sub_type, tier)),
        codex_five_hour_percent: None,
        codex_five_hour_reset: None,
        codex_weekly_percent: None,
        codex_weekly_reset: None,
    })
}

// ---------------------------------------------------------------------------
// Combined poll: probe first, fall back to files
// ---------------------------------------------------------------------------

fn poll_once() -> io::Result<UsageInfo> {
    // Guard: only one probe at a time
    if PROBE_RUNNING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        probe_log("skipped: another probe already running");
        eprintln!("[usage-poll] skipped: another probe already running");
        return poll_from_files();
    }
    let result = poll_once_inner();
    PROBE_RUNNING.store(false, Ordering::SeqCst);
    result
}

fn poll_once_inner() -> io::Result<UsageInfo> {
    // Try the ghost PTY probe first
    match probe_usage() {
        Ok(probe) => {
            eprintln!("[usage-poll] probe succeeded: session={}%, week_all={}%, week_sonnet={}%",
                probe.session_percent.unwrap_or(0),
                probe.week_all_percent.unwrap_or(0),
                probe.week_sonnet_percent.unwrap_or(0),
            );

            // Merge probe results with file-based data for plan_tier etc.
            let (plan_tier, session_messages, session_limit, daily_messages, weekly_messages, messages_limit) =
                match poll_from_files() {
                    Ok(file_info) => (
                        file_info.plan_tier,
                        file_info.session_messages,
                        file_info.session_limit,
                        file_info.daily_messages,
                        file_info.weekly_messages,
                        file_info.messages_limit,
                    ),
                    Err(_) => (None, None, None, None, None, None),
                };

            Ok(UsageInfo {
                raw_output: format!(
                    "Session: {}% | Week (all): {}% | Week (Sonnet): {}%",
                    probe.session_percent.unwrap_or(0),
                    probe.week_all_percent.unwrap_or(0),
                    probe.week_sonnet_percent.unwrap_or(0),
                ),
                session_percent: probe.session_percent,
                session_reset: probe.session_reset,
                week_all_percent: probe.week_all_percent,
                week_all_reset: probe.week_all_reset,
                week_sonnet_percent: probe.week_sonnet_percent,
                week_sonnet_reset: probe.week_sonnet_reset,
                session_messages,
                session_limit,
                daily_messages,
                weekly_messages,
                messages_used: None,
                messages_limit,
                plan_tier,
                codex_five_hour_percent: None,
                codex_five_hour_reset: None,
                codex_weekly_percent: None,
                codex_weekly_reset: None,
            })
        }
        Err(e) => {
            eprintln!("[usage-poll] probe failed: {e}, falling back to file-based");
            poll_from_files()
        }
    }
}

// ---------------------------------------------------------------------------
// Broadcast + public API
// ---------------------------------------------------------------------------

fn store_and_broadcast(state: &Arc<Mutex<SupervisorState>>, info: UsageInfo) {
    let mut merged = info;
    {
        let mut s = state.lock().unwrap();
        if let Some(prev) = s.usage_info.as_ref() {
            if merged.codex_five_hour_percent.is_none() {
                merged.codex_five_hour_percent = prev.codex_five_hour_percent;
            }
            if merged.codex_five_hour_reset.is_none() {
                merged.codex_five_hour_reset = prev.codex_five_hour_reset.clone();
            }
            if merged.codex_weekly_percent.is_none() {
                merged.codex_weekly_percent = prev.codex_weekly_percent;
            }
            if merged.codex_weekly_reset.is_none() {
                merged.codex_weekly_reset = prev.codex_weekly_reset.clone();
            }
        }
        s.usage_info = Some(merged.clone());
    }
    let msg = usage_status_message(&merged);
    broadcast(state, &msg);
}

/// Trigger a one-off usage refresh (manual or on-demand).
pub fn refresh_now(state: Arc<Mutex<SupervisorState>>) {
    thread::spawn(move || {
        match poll_once() {
            Ok(info) => store_and_broadcast(&state, info),
            Err(e) => {
                eprintln!("[usage-poll] manual refresh failed: {e}");
            }
        }
    });
}

/// Start the usage polling thread.  Polls once on startup (after a brief
/// delay), then every 10 minutes.
pub fn start(state: Arc<Mutex<SupervisorState>>) {
    thread::spawn(move || {
        // Initial poll after 5 seconds
        thread::sleep(Duration::from_secs(5));

        loop {
            match poll_once() {
                Ok(info) => store_and_broadcast(&state, info),
                Err(e) => {
                    eprintln!("[usage-poll] failed: {e}");
                }
            }
            thread::sleep(Duration::from_secs(600)); // 10 minutes
        }
    });
}
