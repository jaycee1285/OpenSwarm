use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::agent::types::AgentType;
use crate::app::AppState;
use crate::ipc::proto::AgentEventType;

// ---------------------------------------------------------------------------
// Per-agent dashboard state (accumulated from AgentEvent messages)
// ---------------------------------------------------------------------------

pub struct PendingApproval {
    pub request_id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub description: Option<String>,
}

/// Timestamped cumulative token snapshot for burn rate calculation.
struct TokenSnapshot {
    time: Instant,
    total_tokens: u64, // input + output combined
}

pub struct DashboardState {
    pub model: RefCell<Option<String>>,
    pub session_id: RefCell<Option<String>>,
    pub total_input_tokens: Cell<u64>,
    pub total_output_tokens: Cell<u64>,
    pub total_cost_usd: Cell<f64>,
    pub num_turns: Cell<u64>,
    pub total_duration_ms: Cell<u64>,
    pub active_tool: RefCell<Option<String>>,
    pub tool_start_time: Cell<Option<Instant>>,
    pub thinking: Cell<bool>,
    pub waiting_for_input: Cell<bool>,
    pub pending_approval: RefCell<Option<PendingApproval>>,
    /// Timestamped snapshots for trend calculation.
    token_snapshots: RefCell<Vec<TokenSnapshot>>,
}

impl DashboardState {
    pub fn new() -> Self {
        Self {
            model: RefCell::new(None),
            session_id: RefCell::new(None),
            total_input_tokens: Cell::new(0),
            total_output_tokens: Cell::new(0),
            total_cost_usd: Cell::new(0.0),
            num_turns: Cell::new(0),
            total_duration_ms: Cell::new(0),
            active_tool: RefCell::new(None),
            tool_start_time: Cell::new(None),
            thinking: Cell::new(false),
            waiting_for_input: Cell::new(false),
            pending_approval: RefCell::new(None),
            token_snapshots: RefCell::new(Vec::new()),
        }
    }

    /// Compute burn rate trend: "↑" (increasing), "↓" (decreasing), "~" (steady).
    /// Compares token rate over last 2 minutes vs the 2 minutes before that.
    pub fn token_trend(&self) -> &'static str {
        let snaps = self.token_snapshots.borrow();
        if snaps.len() < 2 {
            return "~";
        }
        let now = Instant::now();
        let mid = now.checked_sub(Duration::from_secs(120)).unwrap_or(now);
        let old = now.checked_sub(Duration::from_secs(240)).unwrap_or(now);

        // Recent window: tokens gained in last 2 minutes
        let recent_start = snaps.iter().rev().find(|s| s.time <= mid);
        let recent_end = snaps.last();

        // Old window: tokens gained 4-2 minutes ago
        let old_start = snaps.iter().rev().find(|s| s.time <= old);

        match (old_start, recent_start, recent_end) {
            (Some(os), Some(rs), Some(re)) => {
                let recent_delta = re.total_tokens.saturating_sub(rs.total_tokens);
                let old_delta = rs.total_tokens.saturating_sub(os.total_tokens);
                if old_delta == 0 && recent_delta == 0 {
                    "~"
                } else if old_delta == 0 {
                    "↑"
                } else {
                    let ratio = recent_delta as f64 / old_delta as f64;
                    if ratio > 1.15 {
                        "↑"
                    } else if ratio < 0.85 {
                        "↓"
                    } else {
                        "~"
                    }
                }
            }
            _ => "~",
        }
    }

    /// Update state from an AgentEvent. Returns true if the dashboard should refresh.
    pub fn apply_event(&self, event: &AgentEventType) -> bool {
        match event {
            AgentEventType::TokenUsage { input_tokens, output_tokens } => {
                self.total_input_tokens.set(self.total_input_tokens.get() + input_tokens);
                self.total_output_tokens.set(self.total_output_tokens.get() + output_tokens);
                // Record snapshot for trend calculation
                let total = self.total_input_tokens.get() + self.total_output_tokens.get();
                let mut snaps = self.token_snapshots.borrow_mut();
                snaps.push(TokenSnapshot { time: Instant::now(), total_tokens: total });
                // Keep only last 5 minutes of snapshots
                let cutoff = Instant::now() - Duration::from_secs(300);
                snaps.retain(|s| s.time >= cutoff);
                true
            }
            AgentEventType::CostUpdate { total_dollars } => {
                self.total_cost_usd.set(*total_dollars);
                true
            }
            AgentEventType::SessionInit { model, session_id } => {
                *self.model.borrow_mut() = Some(model.clone());
                *self.session_id.borrow_mut() = Some(session_id.clone());
                true
            }
            AgentEventType::QueryComplete { num_turns: _, duration_ms, .. } => {
                self.num_turns.set(self.num_turns.get() + 1);
                self.total_duration_ms.set(
                    self.total_duration_ms.get() + duration_ms,
                );
                self.waiting_for_input.set(true);
                *self.active_tool.borrow_mut() = None;
                self.thinking.set(false);
                true
            }
            AgentEventType::ToolStart { tool_name } => {
                *self.active_tool.borrow_mut() = Some(tool_name.clone());
                self.tool_start_time.set(Some(Instant::now()));
                self.thinking.set(false);
                self.waiting_for_input.set(false);
                if self.pending_approval.borrow().is_some() {
                    *self.pending_approval.borrow_mut() = None;
                }
                true
            }
            AgentEventType::ToolEnd { .. } => {
                *self.active_tool.borrow_mut() = None;
                self.tool_start_time.set(None);
                true
            }
            AgentEventType::Thinking => {
                self.thinking.set(true);
                self.waiting_for_input.set(false);
                *self.active_tool.borrow_mut() = None;
                true
            }
            AgentEventType::WaitingForInput => {
                self.waiting_for_input.set(true);
                self.thinking.set(false);
                *self.active_tool.borrow_mut() = None;
                true
            }
            AgentEventType::Error { .. } | AgentEventType::ParentExited { .. } => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Dashboard widgets (right panel)
// ---------------------------------------------------------------------------

pub struct DashboardWidgets {
    pub container: gtk::Box,

    // Usage section (shown only for Anthropic models)
    pub usage_box: gtk::Box,
    pub usage_separator: gtk::Separator,
    pub usage_refresh_btn: gtk::Button,
    pub usage_plan_label: gtk::Label,
    pub usage_row_1: UsageRowWidgets,
    pub usage_row_2: UsageRowWidgets,
    pub usage_row_3: UsageRowWidgets,

    // Agent info
    pub type_label: gtk::Label,
    pub status_label: gtk::Label,
    pub repo_label: gtk::Label,
    pub model_label: gtk::Label,

    // Metrics
    pub cost_label: gtk::Label,
    pub tokens_label: gtk::Label,
    pub turns_label: gtk::Label,
    pub duration_label: gtk::Label,

    // Activity
    pub activity_label: gtk::Label,

    // Approval section
    pub approval_revealer: gtk::Revealer,
    pub approval_tool_label: gtk::Label,
    pub approval_desc_label: gtk::Label,
    pub approve_btn: gtk::Button,
    pub deny_btn: gtk::Button,
}

pub struct UsageRowWidgets {
    pub container: gtk::Box,
    pub label: gtk::Label,
    pub bar: gtk::ProgressBar,
    pub value: gtk::Label,
    pub reset: gtk::Label,
}

fn make_heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_halign(gtk::Align::Center);
    label.set_xalign(0.5);
    label.add_css_class("dashboard-heading");
    label
}

fn make_value(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_halign(gtk::Align::Start);
    label.add_css_class("dashboard-value");
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label
}

pub fn build_right_panel() -> DashboardWidgets {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_width_request(240);
    container.add_css_class("right-panel");

    // Focus indicator
    let right_indicator = gtk::DrawingArea::new();
    right_indicator.set_content_height(3);
    container.append(&right_indicator);

    // --- Usage section (global) ---
    let usage_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    usage_box.set_margin_start(8);
    usage_box.set_margin_end(8);
    usage_box.set_margin_top(8);
    usage_box.set_margin_bottom(4);

    let usage_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let usage_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    usage_spacer.set_hexpand(true);
    usage_header.append(&usage_spacer);

    let usage_refresh_btn = gtk::Button::with_label("Refresh");
    usage_refresh_btn.add_css_class("flat");
    usage_refresh_btn.set_halign(gtk::Align::End);
    usage_header.append(&usage_refresh_btn);
    usage_box.append(&usage_header);

    let usage_title = gtk::Label::new(Some("Usage"));
    usage_title.add_css_class("dashboard-heading");
    usage_title.set_halign(gtk::Align::Center);
    usage_title.set_xalign(0.5);
    usage_box.append(&usage_title);

    let usage_plan_label = make_value("");
    usage_plan_label.add_css_class("usage-plan");
    usage_plan_label.set_visible(false);
    usage_box.append(&usage_plan_label);

    let usage_row_1 = build_usage_row();
    usage_box.append(&usage_row_1.container);
    let usage_row_2 = build_usage_row();
    usage_box.append(&usage_row_2.container);
    let usage_row_3 = build_usage_row();
    usage_box.append(&usage_row_3.container);

    container.append(&usage_box);
    let usage_separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    container.append(&usage_separator);

    // --- Agent info section ---
    let info_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    info_box.set_margin_start(8);
    info_box.set_margin_end(8);
    info_box.set_margin_top(4);
    info_box.set_margin_bottom(4);

    info_box.append(&make_heading("Agent"));

    let type_status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let type_label = make_value("--");
    type_label.add_css_class("agent-type");
    type_status_row.append(&type_label);
    let status_label = make_value("");
    type_status_row.append(&status_label);
    info_box.append(&type_status_row);

    let repo_label = make_value("--");
    repo_label.add_css_class("repo-name");
    info_box.append(&repo_label);

    let model_label = make_value("");
    info_box.append(&model_label);

    container.append(&info_box);
    container.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // --- Metrics section ---
    let metrics_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    metrics_box.set_margin_start(8);
    metrics_box.set_margin_end(8);
    metrics_box.set_margin_top(4);
    metrics_box.set_margin_bottom(4);

    metrics_box.append(&make_heading("Metrics"));

    let cost_label = make_value("Cost: $0.0000");
    metrics_box.append(&cost_label);

    let tokens_label = make_value("Tokens: 0 in / 0 out");
    metrics_box.append(&tokens_label);

    let turns_label = make_value("Turns: 0");
    metrics_box.append(&turns_label);

    let duration_label = make_value("Duration: 0s");
    metrics_box.append(&duration_label);

    container.append(&metrics_box);
    container.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // --- Activity section ---
    let activity_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    activity_box.set_margin_start(8);
    activity_box.set_margin_end(8);
    activity_box.set_margin_top(4);
    activity_box.set_margin_bottom(4);

    activity_box.append(&make_heading("Activity"));

    let activity_label = make_value("Idle");
    activity_box.append(&activity_label);

    container.append(&activity_box);

    // --- Approval section (hidden by default) ---
    let approval_revealer = gtk::Revealer::new();
    approval_revealer.set_reveal_child(false);
    approval_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    approval_revealer.set_transition_duration(200);

    let approval_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    approval_box.add_css_class("approval-box");

    let approval_heading = make_heading("Approval Required");
    approval_box.append(&approval_heading);

    let approval_tool_label = make_value("Tool: --");
    approval_box.append(&approval_tool_label);

    let approval_desc_label = make_value("");
    approval_desc_label.set_wrap(true);
    approval_desc_label.set_max_width_chars(30);
    approval_box.append(&approval_desc_label);

    let approval_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    approval_actions.set_margin_top(4);

    let approve_btn = gtk::Button::with_label("Approve");
    approve_btn.add_css_class("suggested-action");
    approval_actions.append(&approve_btn);

    let deny_btn = gtk::Button::with_label("Deny");
    deny_btn.add_css_class("destructive-action");
    approval_actions.append(&deny_btn);

    approval_box.append(&approval_actions);

    approval_revealer.set_child(Some(&approval_box));
    container.append(&approval_revealer);

    DashboardWidgets {
        container,
        usage_box,
        usage_separator,
        usage_refresh_btn,
        usage_plan_label,
        usage_row_1,
        usage_row_2,
        usage_row_3,
        type_label,
        status_label,
        repo_label,
        model_label,
        cost_label,
        tokens_label,
        turns_label,
        duration_label,
        activity_label,
        approval_revealer,
        approval_tool_label,
        approval_desc_label,
        approve_btn,
        deny_btn,
    }
}

/// Refresh all dashboard labels from the given agent's DashboardState.
pub fn refresh(widgets: &DashboardWidgets, state: &Rc<AppState>) {
    let agents = state.agents.borrow();
    let selected = state.selected_id.get();
    let entry = selected.and_then(|id| agents.iter().find(|a| a.id == id));

    let Some(entry) = entry else {
        clear(widgets);
        return;
    };

    let ds = &entry.dashboard;
    let agent_type = entry.agent_type;

    // Agent info
    widgets.type_label.set_text(entry.agent_type.label());
    widgets.status_label.set_text(match entry.status.get() {
        crate::agent::status::AgentStatus::Running => "Running",
        crate::agent::status::AgentStatus::Idle => "Idle",
        crate::agent::status::AgentStatus::Exited => "Exited",
    });
    widgets.repo_label.set_text(&entry.repo_name);

    // Read RefCell values into locals before they go out of scope
    let model = ds.model.borrow().clone();
    let active_tool = ds.active_tool.borrow().clone();
    let tool_start = ds.tool_start_time.get();
    let in_tok = ds.total_input_tokens.get();
    let out_tok = ds.total_output_tokens.get();
    let turns = ds.num_turns.get();
    let dur_ms = ds.total_duration_ms.get();
    let thinking = ds.thinking.get();
    let waiting = ds.waiting_for_input.get();
    let trend = ds.token_trend();

    // Snapshot approval info
    let approval_info = {
        let pa = ds.pending_approval.borrow();
        pa.as_ref().map(|a| (a.tool_name.clone(), a.description.clone()))
    };

    // Drop the agents borrow before updating widgets
    drop(agents);

    // Usage bar — show for Claude and Codex subscription limits
    let is_anthropic = model.as_ref().map_or(
        // No model info yet — show for ClaudeCode, hide for others
        agent_type == AgentType::ClaudeCode,
        |m| {
            let lower = m.to_lowercase();
            lower.contains("opus")
                || lower.contains("sonnet")
                || lower.contains("haiku")
                || lower.contains("claude")
        },
    );
    let show_usage = is_anthropic || agent_type == AgentType::Codex;
    widgets.usage_box.set_visible(show_usage);
    widgets.usage_separator.set_visible(show_usage);

    // Model
    if let Some(ref m) = model {
        widgets.model_label.set_text(m);
        widgets.model_label.set_visible(true);
    } else {
        widgets.model_label.set_visible(false);
    }

    // Cost — agent-type-specific display
    match agent_type {
        AgentType::OpenCode => {
            // Pay-per-token: calculate dollar cost from rate table
            if let Some(ref m) = model {
                if let Some(cost) = crate::config::calculate_cost(m, in_tok, out_tok) {
                    widgets.cost_label.set_text(&format!("Cost: ${:.4} {}", cost, trend));
                } else {
                    widgets.cost_label.set_text(&format!("Cost: -- {}", trend));
                }
            } else {
                widgets.cost_label.set_text("Cost: --");
            }
        }
        AgentType::ClaudeCode => {
            // Subscription: show "Subscription" — global usage bar handles limits
            widgets.cost_label.set_text("Subscription (see usage bar)");
        }
        AgentType::Codex => {
            // Subscription: no published limits yet
            widgets.cost_label.set_text("Subscription");
        }
    }

    // Tokens — always shown, with trend indicator
    widgets.tokens_label.set_text(&format!(
        "Tokens: {} in / {} out {}",
        format_number(in_tok),
        format_number(out_tok),
        trend,
    ));
    widgets.turns_label.set_text(&format!("Turns: {}", turns));
    let dur_s = dur_ms as f64 / 1000.0;
    widgets.duration_label.set_text(&format!("Duration: {:.1}s", dur_s));

    // Activity
    let activity = if thinking {
        "Thinking...".to_string()
    } else if let Some(ref tool) = active_tool {
        let elapsed = tool_start
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        if elapsed > 0 {
            format!("Tool: {} ({}s)", tool, elapsed)
        } else {
            format!("Tool: {}", tool)
        }
    } else if waiting {
        "Waiting for input".to_string()
    } else {
        "Idle".to_string()
    };
    widgets.activity_label.set_text(&activity);

    // Approval
    let has_approval = approval_info.is_some();
    widgets.approval_revealer.set_reveal_child(has_approval);
    if let Some((tool_name, desc)) = approval_info {
        widgets.approval_tool_label.set_text(&format!("Tool: {}", tool_name));
        if let Some(ref d) = desc {
            widgets.approval_desc_label.set_text(d);
            widgets.approval_desc_label.set_visible(true);
        } else {
            widgets.approval_desc_label.set_visible(false);
        }
    }
}

fn build_usage_row() -> UsageRowWidgets {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 2);
    container.set_margin_bottom(6);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let label = make_value("--");
    label.add_css_class("usage-window-label");
    label.set_width_chars(8);
    let value = make_value("--");
    value.add_css_class("usage-window-value");
    value.set_halign(gtk::Align::End);
    value.set_hexpand(true);
    value.set_xalign(1.0);
    top.append(&label);
    top.append(&value);
    container.append(&top);

    let bar = gtk::ProgressBar::new();
    bar.set_fraction(0.0);
    bar.add_css_class("usage-bar");
    bar.set_show_text(false);
    container.append(&bar);

    let reset = make_value("");
    reset.add_css_class("usage-reset");
    reset.set_halign(gtk::Align::End);
    reset.set_xalign(1.0);
    reset.set_visible(false);
    container.append(&reset);

    UsageRowWidgets {
        container,
        label,
        bar,
        value,
        reset,
    }
}

/// Update usage bar from a UsageStatus message.
pub fn refresh_usage(
    widgets: &DashboardWidgets,
    selected_agent_type: Option<AgentType>,
    raw_output: &str,
    session_percent: Option<u32>,
    session_reset: Option<&str>,
    week_all_percent: Option<u32>,
    week_all_reset: Option<&str>,
    week_sonnet_percent: Option<u32>,
    week_sonnet_reset: Option<&str>,
    session_messages: Option<u32>,
    session_limit: Option<u32>,
    _daily_messages: Option<u32>,
    _weekly_messages: Option<u32>,
    messages_used: Option<u32>,
    messages_limit: Option<u32>,
    plan_tier: Option<&str>,
    codex_five_hour_percent: Option<u32>,
    codex_five_hour_reset: Option<&str>,
    codex_weekly_percent: Option<u32>,
    codex_weekly_reset: Option<&str>,
) {
    clear_usage_rows(widgets);

    if matches!(selected_agent_type, Some(AgentType::Codex)) {
        widgets.usage_plan_label.set_text("Codex");
        widgets.usage_plan_label.set_visible(true);
        set_usage_row(
            &widgets.usage_row_1,
            "5-Hour",
            codex_five_hour_percent,
            codex_five_hour_reset,
        );
        set_usage_row(
            &widgets.usage_row_2,
            "7-Day",
            codex_weekly_percent,
            codex_weekly_reset,
        );
        return;
    }

    // Claude/OpenCode path. Prefer the real /usage probe, then degrade gracefully.
    let session_pct = if let Some(p) = session_percent {
        Some(p)
    } else {
        let used = session_messages
            .or(messages_used)
            .or_else(|| parse_messages_used(raw_output));
        let limit = session_limit.or(messages_limit);
        match (used, limit) {
            (Some(u), Some(l)) if l > 0 => Some(((u.saturating_mul(100)) / l).min(100) as u32),
            _ => None,
        }
    };

    if let Some(tier) = plan_tier {
        if !tier.is_empty() {
            widgets.usage_plan_label.set_text(&format!("Claude {}", tier));
            widgets.usage_plan_label.set_visible(true);
        }
    }

    set_usage_row(
        &widgets.usage_row_1,
        "5-Hour",
        session_pct,
        session_reset,
    );
    set_usage_row(
        &widgets.usage_row_2,
        "7-Day",
        week_all_percent,
        week_all_reset,
    );
    set_usage_row(
        &widgets.usage_row_3,
        "Sonnet",
        week_sonnet_percent,
        week_sonnet_reset,
    );

    if widgets.usage_row_2.container.is_visible()
        && widgets.usage_row_2.reset.text().is_empty()
        && _weekly_messages.is_some()
    {
        widgets
            .usage_row_2
            .reset
            .set_text(&format!("Week msgs: {}", _weekly_messages.unwrap_or(0)));
        widgets.usage_row_2.reset.set_visible(true);
    }
}

fn clear_usage_rows(widgets: &DashboardWidgets) {
    widgets.usage_plan_label.set_visible(false);
    clear_usage_row(&widgets.usage_row_1);
    clear_usage_row(&widgets.usage_row_2);
    clear_usage_row(&widgets.usage_row_3);
}

fn clear_usage_row(row: &UsageRowWidgets) {
    row.container.set_visible(false);
    row.label.set_text("--");
    row.value.set_text("--");
    row.bar.set_fraction(0.0);
    row.bar.remove_css_class("usage-ok");
    row.bar.remove_css_class("usage-warn");
    row.bar.remove_css_class("usage-danger");
    row.reset.set_text("");
    row.reset.set_visible(false);
}

fn set_usage_row(
    row: &UsageRowWidgets,
    label: &str,
    percent_used: Option<u32>,
    reset: Option<&str>,
) {
    let Some(percent_used) = percent_used else {
        clear_usage_row(row);
        return;
    };
    let used = percent_used.min(100);
    let remaining = 100u32.saturating_sub(used);
    row.container.set_visible(true);
    row.label.set_text(label);
    row.value.set_text(&format!("{remaining}% left"));
    row.bar.set_fraction(remaining as f64 / 100.0);
    row.bar.remove_css_class("usage-ok");
    row.bar.remove_css_class("usage-warn");
    row.bar.remove_css_class("usage-danger");
    if used >= 85 {
        row.bar.add_css_class("usage-danger");
    } else if used >= 60 {
        row.bar.add_css_class("usage-warn");
    } else {
        row.bar.add_css_class("usage-ok");
    }
    if let Some(reset) = reset.filter(|s| !s.is_empty()) {
        row.reset.set_text(reset);
        row.reset.set_visible(true);
    } else {
        row.reset.set_text("");
        row.reset.set_visible(false);
    }
}

/// Clear all dashboard labels (no agent selected).
pub fn clear(widgets: &DashboardWidgets) {
    widgets.type_label.set_text("--");
    widgets.status_label.set_text("");
    widgets.repo_label.set_text("--");
    widgets.model_label.set_visible(false);
    widgets.cost_label.set_text("Cost: $0.0000");
    widgets.tokens_label.set_text("Tokens: 0 in / 0 out");
    widgets.turns_label.set_text("Turns: 0");
    widgets.duration_label.set_text("Duration: 0s");
    widgets.activity_label.set_text("Idle");
    widgets.approval_revealer.set_reveal_child(false);
    clear_usage_rows(widgets);
}

fn format_number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn parse_messages_used(raw_output: &str) -> Option<u32> {
    let line = raw_output
        .lines()
        .find(|l| l.contains("Today") && l.contains("messages"))?;
    let after_colon = line.splitn(2, ':').nth(1)?.trim();
    let num_str = after_colon.split_whitespace().next()?;
    let digits: String = num_str.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}
