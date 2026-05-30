use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gtk4 as gtk;
use gtk4::gdk;
use gtk4::prelude::*;
use vte4::prelude::*;

use crate::agent::status::AgentStatus;
use crate::agent::types::AgentType;
use crate::app::AppState;
use crate::ipc;
use crate::ipc::proto::{AgentEventType, ClientMessage, ServerMessage};
use crate::ui::dashboard::{self, DashboardWidgets};
use crate::ui::mobile_dialog::{self, MobileState};
use crate::ui::settings_dialog;
use crate::ui::shortcuts_dialog;
use crate::ui::spawn_dialog;

pub fn apply_status_dot_class(dot: &gtk::Label, status: AgentStatus) {
    dot.remove_css_class("status-running");
    dot.remove_css_class("status-idle");
    dot.remove_css_class("status-exited");
    match status {
        AgentStatus::Running => dot.add_css_class("status-running"),
        AgentStatus::Idle => dot.add_css_class("status-idle"),
        AgentStatus::Exited => dot.add_css_class("status-exited"),
    }
}

fn copy_selected_terminal(state: &Rc<AppState>) {
    if let Some(id) = state.selected_id.get() {
        let agents = state.agents.borrow();
        if let Some(entry) = agents.iter().find(|a| a.id == id) {
            entry.terminal.select_all();
            entry.terminal.copy_clipboard_format(vte4::Format::Text);
            entry.terminal.unselect_all();
        }
    }
}

fn copy_terminal_selection(state: &Rc<AppState>) {
    if let Some(id) = state.selected_id.get() {
        let agents = state.agents.borrow();
        if let Some(entry) = agents.iter().find(|a| a.id == id) {
            if entry.terminal.has_selection() {
                entry.terminal.copy_clipboard_format(vte4::Format::Text);
            } else {
                entry.terminal.select_all();
                entry.terminal.copy_clipboard_format(vte4::Format::Text);
                entry.terminal.unselect_all();
            }
        }
    }
}

fn paste_clipboard_into_selected_agent(state: &Rc<AppState>) {
    let Some(id) = state.selected_id.get() else {
        return;
    };

    let agents = state.agents.borrow();
    let Some(entry) = agents.iter().find(|a| a.id == id) else {
        return;
    };

    let agent_type = entry.agent_type;
    let input_buffer = entry.input_buffer.clone();
    let terminal = entry.terminal.clone();
    drop(agents);

    let clipboard = gdk::Display::default().unwrap().clipboard();
    let ipc = state.ipc.clone();
    clipboard.read_text_async(
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if let Ok(Some(text)) = result {
                if agent_type == crate::agent::types::AgentType::ClaudeCode
                    || agent_type == crate::agent::types::AgentType::Codex
                {
                    input_buffer.borrow_mut().push_str(&text);
                    terminal.feed(text.as_bytes());
                } else {
                    ipc.send(&crate::ipc::proto::ClientMessage::Input {
                        agent_id: id,
                        bytes: text.as_bytes().to_vec(),
                    });
                }
            }
        },
    );
}

pub fn build(app: &gtk::Application) {
    let ipc = match ipc::connect_or_spawn() {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Failed to connect to supervisor: {e}");
            return;
        }
    };
    let ipc = Rc::new(ipc);

    // --- CSS ---
    let css = gtk::CssProvider::new();
    css.load_from_string(
        r#"
        .focus-indicator {
            min-height: 3px;
            background-color: alpha(@accent_bg_color, 0.65);
            opacity: 0;
        }
        .focus-indicator.active {
            opacity: 1;
        }
        .status-dot {
            font-weight: 700;
        }
        .status-dot.status-running {
            color: @success_color;
        }
        .status-dot.status-idle {
            color: @warning_color;
        }
        .status-dot.status-exited {
            color: @error_color;
        }
        .repo-name,
        .empty-label,
        .usage-percent {
            opacity: 0.7;
        }
        .agent-type,
        .dashboard-heading {
            font-weight: 700;
        }
        .approval-box {
            background-color: alpha(@warning_bg_color, 0.12);
            border-radius: 8px;
            padding: 8px;
            margin: 8px;
        }
        button.mobile-button {
            transition: background-color 120ms ease, color 120ms ease, border-color 120ms ease;
        }
        button.mobile-button.mobile-listening {
            background-color: mix(@warning_color, @window_bg_color, 0.18);
            color: @warning_color;
            border-color: @warning_color;
            box-shadow: inset 0 0 0 1px @warning_color;
        }
        button.mobile-button.mobile-listening image {
            color: @warning_color;
            -gtk-icon-style: symbolic;
        }
        button.mobile-button.mobile-connected {
            background-color: mix(@success_color, @window_bg_color, 0.18);
            color: @success_color;
            border-color: @success_color;
            box-shadow: inset 0 0 0 1px @success_color;
        }
        button.mobile-button.mobile-connected image {
            color: @success_color;
            -gtk-icon-style: symbolic;
        }
        .usage-bar {
            min-height: 4px;
        }
        progressbar.usage-bar trough {
            min-height: 8px;
            background-color: alpha(@window_fg_color, 0.12);
            border-radius: 999px;
        }
        progressbar.usage-bar progress {
            min-height: 8px;
            border-radius: 999px;
            background-image: none;
            background-color: #aebfff;
        }
        progressbar.usage-bar.usage-warn progress {
            background-color: #f5c451;
        }
        progressbar.usage-bar.usage-danger progress {
            background-color: #e57373;
        }
        "#,
    );
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // --- Window ---
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("OpenSwarm")
        .default_width(960)
        .default_height(720)
        .build();

    // --- Layout ---
    let main_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    window.set_child(Some(&main_box));

    // --- Active panel indicators ---
    let left_active = Rc::new(Cell::new(false));
    let center_active = Rc::new(Cell::new(false));

    let left_indicator = make_focus_indicator();
    let center_indicator = make_focus_indicator();

    // Left panel
    let left_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left_panel.set_width_request(180);
    left_panel.add_css_class("left-panel");

    left_panel.append(&left_indicator);

    // Button row: spawn + clipboard + settings + mobile
    let button_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    button_row.set_margin_top(4);
    button_row.set_margin_bottom(4);
    button_row.set_margin_start(8);
    button_row.set_margin_end(8);

    let spawn_btn = gtk::Button::with_label("+ New Agent");
    spawn_btn.set_hexpand(true);
    button_row.append(&spawn_btn);

    let copy_btn = gtk::Button::with_label("Copy");
    button_row.append(&copy_btn);

    let paste_btn = gtk::Button::with_label("Paste");
    button_row.append(&paste_btn);

    let settings_btn = gtk::Button::new();
    settings_btn.set_tooltip_text(Some("Terminal Scheme"));
    settings_btn.set_child(Some(&gtk::Image::from_icon_name("emblem-system-symbolic")));
    button_row.append(&settings_btn);

    // Mobile state for tracking WS connection
    let mobile_state = Rc::new(MobileState::default());

    left_panel.append(&button_row);

    // Prior sessions (last 5), toggleable
    let prior_expander = gtk::Expander::new(Some("Prior Sessions"));
    prior_expander.set_margin_start(8);
    prior_expander.set_margin_end(8);
    prior_expander.set_margin_bottom(8);
    let prior_sessions_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    prior_expander.set_child(Some(&prior_sessions_box));
    left_panel.append(&prior_expander);

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let list_box = gtk::ListBox::new();
    list_box.set_selection_mode(gtk::SelectionMode::Single);
    scroll.set_child(Some(&list_box));
    left_panel.append(&scroll);

    main_box.append(&left_panel);

    let sep = gtk::Separator::new(gtk::Orientation::Vertical);
    main_box.append(&sep);

    // Center panel
    let center_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    center_panel.set_hexpand(true);
    center_panel.set_vexpand(true);

    center_panel.append(&center_indicator);

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);

    let empty_label = gtk::Label::new(Some("Ctrl+N to spawn an agent"));
    empty_label.add_css_class("empty-label");
    stack.add_named(&empty_label, Some("empty"));
    stack.set_visible_child_name("empty");

    center_panel.append(&stack);
    main_box.append(&center_panel);

    // --- Right panel (dashboard, hidden by default, Ctrl+R to toggle) ---
    let right_sep = gtk::Separator::new(gtk::Orientation::Vertical);
    let dashboard_widgets = dashboard::build_right_panel();
    dashboard_widgets.container.set_visible(false);
    right_sep.set_visible(false);
    main_box.append(&right_sep);
    main_box.append(&dashboard_widgets.container);

    let dashboard_widgets = Rc::new(dashboard_widgets);

    // --- State ---
    let state = AppState::new(stack.clone(), list_box.clone(), ipc.clone());
    refresh_prior_sessions(&prior_sessions_box, &state);

    // --- Mobile button (add after state is created) ---
    let mobile_btn = mobile_dialog::create_button(&state, &mobile_state, &window);
    button_row.append(&mobile_btn);

    // --- Spawn button ---
    {
        let window_ref = window.clone();
        let state = state.clone();
        spawn_btn.connect_clicked(move |_| {
            spawn_dialog::show(&window_ref, &state);
        });
    }

    {
        let state = state.clone();
        copy_btn.connect_clicked(move |_| {
            copy_selected_terminal(&state);
        });
    }

    {
        let window_ref = window.clone();
        let state = state.clone();
        settings_btn.connect_clicked(move |_| {
            settings_dialog::show(&window_ref, &state);
        });
    }

    {
        let state = state.clone();
        paste_btn.connect_clicked(move |_| {
            paste_clipboard_into_selected_agent(&state);
        });
    }

    // --- Usage refresh button ---
    {
        let ipc = state.ipc.clone();
        let refresh_btn = dashboard_widgets.usage_refresh_btn.clone();
        refresh_btn.connect_clicked(move |_| {
            ipc.send(&crate::ipc::proto::ClientMessage::RefreshUsage);
        });
    }
    // Scope switch should be a local view change only; polling on every
    // changed signal can create refresh storms when the scope model is rebuilt.

    // --- IPC receiver ---
    {
        let state = state.clone();
        let stack = stack.clone();
        let ipc = ipc.clone();
        let mobile_state = mobile_state.clone();
        let dw = dashboard_widgets.clone();
        let right_sep = right_sep.clone();
        let prior_sessions_box = prior_sessions_box.clone();
        let window_for_ipc = window.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            while let Some(msg) = ipc.try_recv() {
                handle_server_message(
                    &window_for_ipc,
                    &state,
                    &stack,
                    &mobile_state,
                    &dw,
                    &right_sep,
                    &prior_sessions_box,
                    msg,
                );
            }
            check_terminal_resizes(&state);
            glib::ControlFlow::Continue
        });
    }

    // --- Row selection ---
    {
        let state = state.clone();
        let dw = dashboard_widgets.clone();
        list_box.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let idx = row.index() as usize;
                // Pause output for the previously selected agent
                if let Some(prev_id) = state.selected_id.get() {
                    state.ipc.send(
                        &crate::ipc::proto::ClientMessage::SetOutputPaused {
                            agent_id: prev_id,
                            paused: true,
                        },
                    );
                }
                let agents = state.agents.borrow();
                if let Some(entry) = agents.get(idx) {
                    state.selected_id.set(Some(entry.id));
                    state.stack.set_visible_child_name(&entry.id.to_string());
                    // Unpause output for the newly selected agent
                    state.ipc.send(
                        &crate::ipc::proto::ClientMessage::SetOutputPaused {
                            agent_id: entry.id,
                            paused: false,
                        },
                    );
                    entry.terminal.grab_focus();
                }
                drop(agents);
                dashboard::refresh(&dw, &state);
                refresh_usage_from_snapshot(&state, &dw);
            }
        });
    }

    // --- Focus tracking for panel indicators ---
    {
        let la = left_active.clone();
        let ca = center_active.clone();
        let li = left_indicator.clone();
        let ci = center_indicator.clone();
        left_panel.connect_state_flags_changed(move |w, prev| {
            let was = prev.contains(gtk::StateFlags::FOCUS_WITHIN);
            let is = w.state_flags().contains(gtk::StateFlags::FOCUS_WITHIN);
            if was != is {
                la.set(is);
                if is { ca.set(false); }
                set_focus_indicator_active(&li, la.get());
                set_focus_indicator_active(&ci, ca.get());
            }
        });
    }
    {
        let la = left_active.clone();
        let ca = center_active.clone();
        let li = left_indicator.clone();
        let ci = center_indicator.clone();
        center_panel.connect_state_flags_changed(move |w, prev| {
            let was = prev.contains(gtk::StateFlags::FOCUS_WITHIN);
            let is = w.state_flags().contains(gtk::StateFlags::FOCUS_WITHIN);
            if was != is {
                ca.set(is);
                if is { la.set(false); }
                set_focus_indicator_active(&li, la.get());
                set_focus_indicator_active(&ci, ca.get());
            }
        });
    }

    // --- Approval button handlers ---
    {
        let state = state.clone();
        let approve_btn = dashboard_widgets.approve_btn.clone();
        let dw = dashboard_widgets.clone();
        approve_btn.connect_clicked(move |_| {
            if let Some(id) = state.selected_id.get() {
                let approval_data = {
                    let agents = state.agents.borrow();
                    agents.iter().find(|a| a.id == id).and_then(|entry| {
                        let pa = entry.dashboard.pending_approval.borrow();
                        pa.as_ref().map(|a| a.request_id.clone())
                    })
                };
                if let Some(request_id) = approval_data {
                    state.ipc.send(&crate::ipc::proto::ClientMessage::ToolApprovalResponse {
                        agent_id: id,
                        request_id,
                        approved: true,
                        updated_input: None,
                    });
                    let agents = state.agents.borrow();
                    if let Some(entry) = agents.iter().find(|a| a.id == id) {
                        *entry.dashboard.pending_approval.borrow_mut() = None;
                        entry.dashboard.waiting_for_input.set(false);
                    }
                    drop(agents);
                    dashboard::refresh(&dw, &state);
                }
            }
        });
    }

    {
        let state = state.clone();
        let deny_btn = dashboard_widgets.deny_btn.clone();
        let dw = dashboard_widgets.clone();
        deny_btn.connect_clicked(move |_| {
            if let Some(id) = state.selected_id.get() {
                let approval_data = {
                    let agents = state.agents.borrow();
                    agents.iter().find(|a| a.id == id).and_then(|entry| {
                        let pa = entry.dashboard.pending_approval.borrow();
                        pa.as_ref().map(|a| a.request_id.clone())
                    })
                };
                if let Some(request_id) = approval_data {
                    state.ipc.send(&crate::ipc::proto::ClientMessage::ToolApprovalResponse {
                        agent_id: id,
                        request_id,
                        approved: false,
                        updated_input: None,
                    });
                    let agents = state.agents.borrow();
                    if let Some(entry) = agents.iter().find(|a| a.id == id) {
                        *entry.dashboard.pending_approval.borrow_mut() = None;
                        entry.dashboard.waiting_for_input.set(false);
                    }
                    drop(agents);
                    dashboard::refresh(&dw, &state);
                }
            }
        });
    }

    // --- Dashboard refresh timer (update tool elapsed time) ---
    {
        let state = state.clone();
        let dw = dashboard_widgets.clone();
        glib::timeout_add_seconds_local(1, move || {
            // Only refresh if an agent is selected and has an active tool
            if let Some(id) = state.selected_id.get() {
                let has_active_tool = {
                    let agents = state.agents.borrow();
                    agents.iter().find(|a| a.id == id)
                        .map(|e| e.dashboard.active_tool.borrow().is_some())
                        .unwrap_or(false)
                };
                if has_active_tool {
                    dashboard::refresh(&dw, &state);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // --- Keyboard shortcuts ---
    setup_shortcuts(&window, &state, &dashboard_widgets, &right_sep);

    // --- Status timer ---
    setup_status_timer(&state);

    window.present();
}

fn handle_server_message(
    window: &gtk::ApplicationWindow,
    state: &Rc<AppState>,
    stack: &gtk::Stack,
    mobile_state: &Rc<MobileState>,
    dw: &Rc<DashboardWidgets>,
    right_sep: &gtk::Separator,
    prior_sessions_box: &gtk::Box,
    msg: ServerMessage,
) {
    match msg {
        ServerMessage::Welcome { .. } => {
            // Consumed during connect handshake; should not appear here
        }
        ServerMessage::ModelCatalog { .. } => {
            // Desktop spawn UI already reads from local config.rs.
        }
        ServerMessage::RepoInventory { repos } => {
            *state.repo_inventory.borrow_mut() = repos;
        }
        ServerMessage::AgentList { agents } => {
            for info in agents {
                let found = {
                    let agents_ref = state.agents.borrow();
                    if let Some(entry) = agents_ref.iter().find(|a| a.id == info.id) {
                        entry.status.set(info.status);
                        apply_status_dot_class(&entry.status_dot, info.status);
                        true
                    } else {
                        false
                    }
                };
                if !found {
                    state.add_agent_from_info(info);
                }
            }
            if state.selected_id.get().is_none() {
                if let Some(entry) = state.agents.borrow().first() {
                    state.list_box.select_row(Some(&entry.row));
                } else {
                    stack.set_visible_child_name("empty");
                }
            }
            refresh_prior_sessions(prior_sessions_box, state);
        }
        ServerMessage::RecentSessions { sessions } => {
            *state.recent_sessions.borrow_mut() = sessions;
            refresh_prior_sessions(prior_sessions_box, state);
        }
        ServerMessage::AgentOutput { agent_id, bytes } => {
            let agents = state.agents.borrow();
            if let Some(entry) = agents.iter().find(|a| a.id == agent_id) {
                entry.terminal.feed(&bytes);
                entry.last_output.set(Instant::now());
                if entry.status.get() != AgentStatus::Exited {
                    entry.status.set(AgentStatus::Running);
                    apply_status_dot_class(&entry.status_dot, AgentStatus::Running);
                }
            }
        }
        ServerMessage::AgentStatus { agent_id, status } => {
            let agents = state.agents.borrow();
            if let Some(entry) = agents.iter().find(|a| a.id == agent_id) {
                entry.status.set(status);
                apply_status_dot_class(&entry.status_dot, status);
            }
        }
        ServerMessage::AgentEvent { agent_id, timestamp: _, event } => {
            // Handle structured events from agents
            // For now, just log them - UI enhancements can be added later
            match &event {
                AgentEventType::ToolStart { tool_name } => {
                    eprintln!("[agent {}] tool start: {}", agent_id, tool_name);
                }
                AgentEventType::ToolEnd { tool_name, success, duration_ms } => {
                    eprintln!("[agent {}] tool end: {} (success={}, {}ms)", agent_id, tool_name, success, duration_ms);
                }
                AgentEventType::CostUpdate { total_dollars } => {
                    eprintln!("[agent {}] cost: ${:.4}", agent_id, total_dollars);
                }
                AgentEventType::Thinking => {
                    // Could update UI to show thinking indicator
                }
                AgentEventType::WaitingForInput => {
                    eprintln!("[agent {}] waiting for input", agent_id);
                }
                AgentEventType::Error { message } => {
                    eprintln!("[agent {}] error: {}", agent_id, message);
                }
                AgentEventType::TokenUsage { input_tokens, output_tokens } => {
                    eprintln!("[agent {}] tokens: {} in, {} out", agent_id, input_tokens, output_tokens);
                }
                AgentEventType::ParentExited { parent_id } => {
                    eprintln!("[agent {}] parent {} exited", agent_id, parent_id);
                }
                AgentEventType::SessionInit { model, session_id } => {
                    eprintln!("[agent {}] session init: model={} session={}", agent_id, model, session_id);
                }
                AgentEventType::QueryComplete { num_turns, duration_ms, is_error } => {
                    eprintln!("[agent {}] query complete: turns={} {}ms error={}", agent_id, num_turns, duration_ms, is_error);
                }
            }

            // Accumulate into dashboard state and refresh if selected
            {
                let agents = state.agents.borrow();
                if let Some(entry) = agents.iter().find(|a| a.id == agent_id) {
                    entry.dashboard.apply_event(&event);
                }
            }
            if state.selected_id.get() == Some(agent_id) {
                dashboard::refresh(dw, state);
            }
        }
        ServerMessage::WsStatus { enabled, connected_peers } => {
            mobile_state.enabled.set(enabled);
            *mobile_state.connected_peers.borrow_mut() = connected_peers;
            mobile_state.update_button();
        }
        ServerMessage::ToolApprovalRequest { agent_id, request_id, tool_name, tool_input, description } => {
            // Store pending approval in dashboard state
            let agents = state.agents.borrow();
            if let Some(entry) = agents.iter().find(|a| a.id == agent_id) {
                *entry.dashboard.pending_approval.borrow_mut() = Some(
                    crate::ui::dashboard::PendingApproval {
                        request_id: request_id.clone(),
                        tool_name: tool_name.clone(),
                        tool_input,
                        description,
                    }
                );
                state.list_box.select_row(Some(&entry.row));
            }
            drop(agents);
            eprintln!("[ui] tool approval pending: {} for agent {}", tool_name, agent_id);
            right_sep.set_visible(true);
            dw.container.set_visible(true);
            dashboard::refresh(dw, state);
        }
        ServerMessage::QuestionRequest { agent_id, request_id, questions } => {
            eprintln!("[ui] question request pending for agent {}", agent_id);
            show_question_dialog(window, state, agent_id, request_id, questions);
        }
        ServerMessage::UsageStatus {
            raw_output,
            session_percent,
            session_reset,
            week_all_percent,
            week_all_reset,
            week_sonnet_percent,
            week_sonnet_reset,
            session_messages,
            session_limit,
            daily_messages,
            weekly_messages,
            messages_used,
            messages_limit,
            plan_tier,
            codex_five_hour_percent,
            codex_five_hour_reset,
            codex_weekly_percent,
            codex_weekly_reset,
        } => {
            *state.usage_snapshot.borrow_mut() = Some(crate::app::UsageSnapshot {
                raw_output,
                session_percent,
                session_reset,
                week_all_percent,
                week_all_reset,
                week_sonnet_percent,
                week_sonnet_reset,
                session_messages,
                session_limit,
                daily_messages,
                weekly_messages,
                messages_used,
                messages_limit,
                plan_tier,
                codex_five_hour_percent,
                codex_five_hour_reset,
                codex_weekly_percent,
                codex_weekly_reset,
            });
            refresh_usage_from_snapshot(state, dw);
        }
    }
}

fn selected_agent_type(state: &Rc<AppState>) -> Option<AgentType> {
    let agents = state.agents.borrow();
    state
        .selected_id
        .get()
        .and_then(|id| agents.iter().find(|a| a.id == id))
        .map(|a| a.agent_type)
}

fn refresh_usage_from_snapshot(state: &Rc<AppState>, dw: &Rc<DashboardWidgets>) {
    let snapshot = state.usage_snapshot.borrow().clone();
    if let Some(s) = snapshot {
        dashboard::refresh_usage(
            dw,
            selected_agent_type(state),
            &s.raw_output,
            s.session_percent,
            s.session_reset.as_deref(),
            s.week_all_percent,
            s.week_all_reset.as_deref(),
            s.week_sonnet_percent,
            s.week_sonnet_reset.as_deref(),
            s.session_messages,
            s.session_limit,
            s.daily_messages,
            s.weekly_messages,
            s.messages_used,
            s.messages_limit,
            s.plan_tier.as_deref(),
            s.codex_five_hour_percent,
            s.codex_five_hour_reset.as_deref(),
            s.codex_weekly_percent,
            s.codex_weekly_reset.as_deref(),
        );
    }
}

fn refresh_prior_sessions(container: &gtk::Box, state: &Rc<AppState>) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let sessions = state.recent_sessions.borrow().clone();

    if sessions.is_empty() {
        let label = gtk::Label::new(Some("No prior sessions yet."));
        label.set_xalign(0.0);
        label.add_css_class("repo-name");
        container.append(&label);
        return;
    }

    for s in sessions.iter().take(5) {
        let text = format!("{} · {} · {}", s.date_mmdd, s.repo_name, s.agent_type.label());
        let item = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let label = gtk::Label::new(Some(&text));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.add_css_class("repo-name");
        row.append(&label);

        let resume_btn = gtk::Button::with_label("Resume");
        resume_btn.add_css_class("flat");
        resume_btn.set_sensitive(s.can_resume);
        let ipc = state.ipc.clone();
        let agent_id = s.id;
        let agent_type = s.agent_type;
        let repo_path = s.repo_path.clone();
        let session_handle = s.session_handle.clone();
        resume_btn.connect_clicked(move |_| {
            if let Some(session_handle) = session_handle.clone() {
                ipc.send(&ClientMessage::ResumeExportedSession {
                    agent_type,
                    repo_path: repo_path.clone(),
                    session_handle,
                });
            } else if agent_id != 0 {
                ipc.send(&ClientMessage::ResumeAgent { agent_id });
            }
        });
        row.append(&resume_btn);
        item.append(&row);

        if s.last_user_message.is_some() || s.last_agent_message.is_some() {
            let expander = gtk::Expander::new(Some("Context"));
            let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
            if let Some(user) = s.last_user_message.clone() {
                let lbl = gtk::Label::new(Some(&format!("User: {}", user)));
                lbl.set_wrap(true);
                lbl.set_xalign(0.0);
                lbl.add_css_class("repo-name");
                content.append(&lbl);
            }
            if let Some(agent) = s.last_agent_message.clone() {
                let lbl = gtk::Label::new(Some(&format!("Agent: {}", agent)));
                lbl.set_wrap(true);
                lbl.set_xalign(0.0);
                lbl.add_css_class("repo-name");
                content.append(&lbl);
            }
            let marker = gtk::Label::new(Some("[prior to resuming]"));
            marker.set_xalign(0.0);
            marker.add_css_class("repo-name");
            content.append(&marker);
            expander.set_child(Some(&content));
            item.append(&expander);
        }

        container.append(&item);
    }
}

fn show_question_dialog(
    window: &gtk::ApplicationWindow,
    state: &Rc<AppState>,
    agent_id: u32,
    request_id: String,
    questions: serde_json::Value,
) {
    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title(format!("Agent {} Question", agent_id))
        .default_width(560)
        .build();
    dialog.add_button("Reject", gtk::ResponseType::Reject);
    dialog.add_button("Submit", gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);

    let content = dialog.content_area();
    content.set_spacing(8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let entries: Rc<RefCell<Vec<gtk::Entry>>> = Rc::new(RefCell::new(Vec::new()));

    let items = questions.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        let label = gtk::Label::new(Some("OpenCode requested input, but no question payload was provided."));
        label.set_wrap(true);
        label.set_xalign(0.0);
        content.append(&label);
    } else {
        for (idx, q) in items.iter().enumerate() {
            let box_q = gtk::Box::new(gtk::Orientation::Vertical, 4);

            let header = q
                .get("header")
                .and_then(|v| v.as_str())
                .unwrap_or("Question");
            let text = q
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let title = gtk::Label::new(Some(&format!("{} {}", header, if text.is_empty() { "" } else { "—" })));
            title.set_xalign(0.0);
            title.add_css_class("dashboard-heading");
            box_q.append(&title);

            if !text.is_empty() {
                let qlabel = gtk::Label::new(Some(text));
                qlabel.set_wrap(true);
                qlabel.set_xalign(0.0);
                box_q.append(&qlabel);
            }

            let options = q
                .get("options")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let default_answer = options
                .first()
                .and_then(|o| o.get("label"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let entry = gtk::Entry::new();
            entry.set_hexpand(true);
            entry.set_placeholder_text(Some("Answer labels (comma-separated)"));
            if !default_answer.is_empty() {
                entry.set_text(default_answer);
            }
            box_q.append(&entry);
            entries.borrow_mut().push(entry);

            if !options.is_empty() {
                let mut options_text = String::new();
                for (opt_i, opt) in options.iter().enumerate() {
                    if opt_i > 0 {
                        options_text.push('\n');
                    }
                    let label = opt.get("label").and_then(|v| v.as_str()).unwrap_or("");
                    let desc = opt
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    options_text.push_str("- ");
                    options_text.push_str(label);
                    if !desc.is_empty() {
                        options_text.push_str(": ");
                        options_text.push_str(desc);
                    }
                }
                let hint = gtk::Label::new(Some(&options_text));
                hint.set_wrap(true);
                hint.set_xalign(0.0);
                hint.add_css_class("dim-label");
                box_q.append(&hint);
            }

            if idx + 1 < items.len() {
                box_q.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            }
            content.append(&box_q);
        }
    }

    let ipc = state.ipc.clone();
    dialog.connect_response(move |d, resp| {
        match resp {
            gtk::ResponseType::Accept => {
                let answers = entries
                    .borrow()
                    .iter()
                    .map(|e| {
                        e.text()
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                ipc.send(&ClientMessage::QuestionResponse {
                    agent_id,
                    request_id: request_id.clone(),
                    answers,
                    rejected: false,
                });
            }
            _ => {
                ipc.send(&ClientMessage::QuestionResponse {
                    agent_id,
                    request_id: request_id.clone(),
                    answers: Vec::new(),
                    rejected: true,
                });
            }
        }
        d.close();
    });

    dialog.present();
}

fn setup_shortcuts(
    window: &gtk::ApplicationWindow,
    state: &Rc<AppState>,
    dashboard_widgets: &Rc<DashboardWidgets>,
    right_sep: &gtk::Separator,
) {
    let key_controller = gtk::EventControllerKey::new();

    // Capture phase so we intercept before VTE eats the keys
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    let state = state.clone();
    let window_for_closure = window.clone();
    let dw = dashboard_widgets.clone();
    let rsep = right_sep.clone();
    key_controller.connect_key_pressed(move |_, key, _code, modifiers| {
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);

        if ctrl && !shift {
            match key {
                gdk::Key::r => {
                    state.resume_selected();
                    return glib::Propagation::Stop;
                }
                gdk::Key::Right => {
                    let visible = !dw.container.is_visible();
                    dw.container.set_visible(visible);
                    rsep.set_visible(visible);
                    if visible {
                        dashboard::refresh(&dw, &state);
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::n => {
                    spawn_dialog::show(
                        &window_for_closure,
                        &state,
                    );
                    return glib::Propagation::Stop;
                }
                gdk::Key::w => {
                    state.kill_selected();
                    return glib::Propagation::Stop;
                }
                gdk::Key::Tab => {
                    state.select_next();
                    return glib::Propagation::Stop;
                }
                gdk::Key::l | gdk::Key::Left => {
                    state.list_box.grab_focus();
                    return glib::Propagation::Stop;
                }
                gdk::Key::e | gdk::Key::Up => {
                    if let Some(id) = state.selected_id.get() {
                        let agents = state.agents.borrow();
                        if let Some(entry) = agents.iter().find(|a| a.id == id) {
                            entry.terminal.grab_focus();
                        }
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::u => {
                    state.ipc.send(&crate::ipc::proto::ClientMessage::RefreshUsage);
                    return glib::Propagation::Stop;
                }
                gdk::Key::h | gdk::Key::k => {
                    shortcuts_dialog::show(&window_for_closure);
                    return glib::Propagation::Stop;
                }
                gdk::Key::_1 => { state.select_by_index(0); return glib::Propagation::Stop; }
                gdk::Key::_2 => { state.select_by_index(1); return glib::Propagation::Stop; }
                gdk::Key::_3 => { state.select_by_index(2); return glib::Propagation::Stop; }
                gdk::Key::_4 => { state.select_by_index(3); return glib::Propagation::Stop; }
                gdk::Key::_5 => { state.select_by_index(4); return glib::Propagation::Stop; }
                gdk::Key::_6 => { state.select_by_index(5); return glib::Propagation::Stop; }
                gdk::Key::_7 => { state.select_by_index(6); return glib::Propagation::Stop; }
                gdk::Key::_8 => { state.select_by_index(7); return glib::Propagation::Stop; }
                gdk::Key::_9 => { state.select_by_index(8); return glib::Propagation::Stop; }
                _ => {}
            }
        }

        if ctrl && shift {
            match key {
                gdk::Key::ISO_Left_Tab | gdk::Key::Tab => {
                    state.select_prev();
                    return glib::Propagation::Stop;
                }
                gdk::Key::W => {
                    state.remove_selected();
                    return glib::Propagation::Stop;
                }
                gdk::Key::C => {
                    copy_terminal_selection(&state);
                    return glib::Propagation::Stop;
                }
                gdk::Key::V => {
                    paste_clipboard_into_selected_agent(&state);
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
        }

        glib::Propagation::Proceed
    });

    window.add_controller(key_controller);
}

fn make_focus_indicator() -> gtk::Box {
    let indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    indicator.set_height_request(3);
    indicator.set_margin_top(6);
    indicator.set_margin_bottom(2);
    indicator.add_css_class("focus-indicator");
    indicator
}

fn set_focus_indicator_active(indicator: &gtk::Box, active: bool) {
    if active {
        indicator.add_css_class("active");
    } else {
        indicator.remove_css_class("active");
    }
}

fn check_terminal_resizes(state: &Rc<AppState>) {
    let agents = state.agents.borrow();
    for entry in agents.iter() {
        let cols = entry.terminal.column_count();
        let rows = entry.terminal.row_count();
        if cols != entry.last_pty_cols.get() || rows != entry.last_pty_rows.get() {
            entry.last_pty_cols.set(cols);
            entry.last_pty_rows.set(rows);
            if cols > 0 && rows > 0 {
                state.ipc.send(&crate::ipc::proto::ClientMessage::ResizeAgent {
                    agent_id: entry.id,
                    rows: rows as u16,
                    cols: cols as u16,
                });
            }
        }
    }
}

fn setup_status_timer(state: &Rc<AppState>) {
    let state = state.clone();
    glib::timeout_add_seconds_local(1, move || {
        let agents = state.agents.borrow();
        let now = Instant::now();
        for entry in agents.iter() {
            if entry.status.get() == AgentStatus::Exited {
                continue;
            }
            let elapsed = now.duration_since(entry.last_output.get());
            let new_status = if elapsed.as_secs() < 10 {
                AgentStatus::Running
            } else {
                AgentStatus::Idle
            };
            if new_status != entry.status.get() {
                entry.status.set(new_status);
                apply_status_dot_class(&entry.status_dot, new_status);
            }
        }
        glib::ControlFlow::Continue
    });
}
