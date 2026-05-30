use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gio, glib};

use crate::agent::types::AgentType;
use crate::app::AppState;
use crate::config::{self, PROMPT_TEMPLATES};
use crate::ipc::proto::{ClientMessage, RepoInfo};

pub fn show(parent: &gtk::ApplicationWindow, state: &Rc<AppState>) {
    let dialog = gtk::Window::builder()
        .title("Spawn New Agent")
        .modal(true)
        .transient_for(parent)
        .default_width(420)
        .default_height(360)
        .resizable(false)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    dialog.set_child(Some(&content));

    // --- Agent type ---
    let type_label = gtk::Label::new(Some("Agent Type"));
    type_label.set_halign(gtk::Align::Start);
    type_label.add_css_class("heading");
    content.append(&type_label);

    let type_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let claude_radio = gtk::CheckButton::with_label("Claude Code");
    claude_radio.set_active(true);
    let codex_radio = gtk::CheckButton::with_label("Codex");
    codex_radio.set_group(Some(&claude_radio));
    let opencode_radio = gtk::CheckButton::with_label("OpenCode");
    opencode_radio.set_group(Some(&claude_radio));
    type_box.append(&claude_radio);
    type_box.append(&codex_radio);
    type_box.append(&opencode_radio);
    content.append(&type_box);

    // --- Model ---
    let model_label = gtk::Label::new(Some("Model"));
    model_label.set_halign(gtk::Align::Start);
    model_label.add_css_class("heading");
    content.append(&model_label);

    let model_dropdown = gtk::DropDown::new(None::<gtk::StringList>, None::<gtk::Expression>);
    content.append(&model_dropdown);

    // Populate model dropdown based on agent type
    let populate_models = {
        let model_dropdown = model_dropdown.clone();
        move |agent_type: AgentType| {
            let options = config::model_options(agent_type);
            let labels: Vec<&str> = options.iter().map(|(_, label)| *label).collect();
            let string_list = gtk::StringList::new(&labels);
            model_dropdown.set_model(Some(&string_list));
            // Select the default
            let default = config::default_model(agent_type);
            if let Some(idx) = options.iter().position(|(id, _)| *id == default) {
                model_dropdown.set_selected(idx as u32);
            }
        }
    };

    // Initial population for Claude (default selection)
    populate_models(AgentType::ClaudeCode);

    // Update model list when agent type changes
    {
        let populate = populate_models.clone();
        claude_radio.connect_toggled(move |btn| {
            if btn.is_active() { populate(AgentType::ClaudeCode); }
        });
    }
    {
        let populate = populate_models.clone();
        codex_radio.connect_toggled(move |btn| {
            if btn.is_active() { populate(AgentType::Codex); }
        });
    }
    {
        let populate = populate_models.clone();
        opencode_radio.connect_toggled(move |btn| {
            if btn.is_active() { populate(AgentType::OpenCode); }
        });
    }

    // --- Repository ---
    let repo_heading = gtk::Label::new(Some("Repository"));
    repo_heading.set_halign(gtk::Align::Start);
    repo_heading.add_css_class("heading");
    content.append(&repo_heading);

    let repo_inventory = state.repo_inventory.borrow().clone();
    let repo_options: Vec<String> = repo_inventory
        .iter()
        .map(|repo| format!("{}  {}", repo.repo_name, repo.repo_path))
        .collect();
    let repo_strings = gtk::StringList::new(
        &repo_options
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
    );
    let repo_dropdown = gtk::DropDown::new(Some(repo_strings), None::<gtk::Expression>);
    repo_dropdown.set_hexpand(true);
    repo_dropdown.set_sensitive(!repo_inventory.is_empty());
    if !repo_inventory.is_empty() {
        repo_dropdown.set_selected(0);
    }

    let repo_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    repo_box.append(&repo_dropdown);
    let browse_btn = gtk::Button::with_label("Browse…");
    repo_box.append(&browse_btn);
    content.append(&repo_box);

    let repo_hint = gtk::Label::new(if repo_inventory.is_empty() {
        Some("No repositories discovered under ~/repos yet. Use Browse as fallback.")
    } else {
        Some("Choose from ~/repos or override with Browse.")
    });
    repo_hint.set_xalign(0.0);
    repo_hint.add_css_class("repo-name");
    repo_hint.set_wrap(true);
    content.append(&repo_hint);

    let selected_repo: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));

    {
        let dialog_ref = dialog.clone();
        let repo_hint = repo_hint.clone();
        let selected_repo = selected_repo.clone();
        browse_btn.connect_clicked(move |_| {
            let file_dialog = gtk::FileDialog::new();
            file_dialog.set_title("Select Repository");
            let home = std::env::var("HOME").unwrap_or_else(|_| "/home".to_string());
            let repos_dir = PathBuf::from(&home).join("repos");
            let initial = gio::File::for_path(&repos_dir);
            file_dialog.set_initial_folder(Some(&initial));

            let repo_hint = repo_hint.clone();
            let selected_repo = selected_repo.clone();
            file_dialog.select_folder(
                Some(&dialog_ref),
                None::<&gio::Cancellable>,
                move |result: Result<gio::File, glib::Error>| {
                    if let Ok(folder) = result {
                        if let Some(path) = folder.path() {
                            repo_hint.set_text(&format!(
                                "Browse override: {}",
                                path.to_string_lossy()
                            ));
                            *selected_repo.borrow_mut() = Some(path);
                        }
                    }
                },
            );
        });
    }

    // --- Prompt ---
    let prompt_heading = gtk::Label::new(Some("Prompt"));
    prompt_heading.set_halign(gtk::Align::Start);
    prompt_heading.add_css_class("heading");
    content.append(&prompt_heading);

    let mut template_labels: Vec<String> = PROMPT_TEMPLATES
        .iter()
        .map(|(label, _)| label.to_string())
        .collect();
    template_labels.push("Custom…".to_string());

    let string_list =
        gtk::StringList::new(&template_labels.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let dropdown = gtk::DropDown::new(Some(string_list), None::<gtk::Expression>);
    content.append(&dropdown);

    let custom_entry = gtk::Entry::new();
    custom_entry.set_placeholder_text(Some("Enter custom prompt…"));
    let custom_revealer = gtk::Revealer::new();
    custom_revealer.set_child(Some(&custom_entry));
    custom_revealer.set_reveal_child(false);
    content.append(&custom_revealer);

    {
        let custom_revealer = custom_revealer.clone();
        let custom_idx = template_labels.len() as u32 - 1;
        dropdown.connect_selected_notify(move |dd| {
            custom_revealer.set_reveal_child(dd.selected() == custom_idx);
        });
    }

    // --- Buttons ---
    let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    button_box.set_halign(gtk::Align::End);
    button_box.set_margin_top(8);
    let cancel_btn = gtk::Button::with_label("Cancel");
    let launch_btn = gtk::Button::with_label("Launch");
    launch_btn.add_css_class("suggested-action");
    button_box.append(&cancel_btn);
    button_box.append(&launch_btn);
    content.append(&button_box);

    // --- Cancel ---
    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            dialog.close();
        });
    }

    // --- Launch ---
    {
        let dialog = dialog.clone();
        let state = state.clone();
        let claude_radio = claude_radio.clone();
        let codex_radio = codex_radio.clone();
        let repo_inventory = repo_inventory.clone();
        let repo_dropdown = repo_dropdown.clone();
        let selected_repo = selected_repo.clone();
        let dropdown = dropdown.clone();
        let custom_entry = custom_entry.clone();
        let model_dropdown = model_dropdown.clone();

        launch_btn.connect_clicked(move |_| {
            // Determine agent type
            let agent_type = if claude_radio.is_active() {
                AgentType::ClaudeCode
            } else if codex_radio.is_active() {
                AgentType::Codex
            } else {
                AgentType::OpenCode
            };

            // Determine model
            let model_options = config::model_options(agent_type);
            let model_idx = model_dropdown.selected() as usize;
            let model = model_options.get(model_idx).map(|(id, _)| id.to_string());

            // Determine repo
            let repo_path = selected_repo.borrow().clone().or_else(|| {
                let idx = repo_dropdown.selected() as usize;
                repo_inventory
                    .get(idx)
                    .map(|repo: &RepoInfo| PathBuf::from(&repo.repo_path))
            });
            let repo_path = match repo_path {
                Some(path) => path,
                None => {
                    eprintln!("No repository selected");
                    return;
                }
            };
            // Determine prompt
            let selected = dropdown.selected() as usize;
            let prompt = if selected < PROMPT_TEMPLATES.len() {
                PROMPT_TEMPLATES[selected].1.map(|s| s.to_string())
            } else {
                // Custom
                let text = custom_entry.text().to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            };

            state.ipc.send(&ClientMessage::SpawnAgent {
                agent_type,
                repo_path: repo_path.to_string_lossy().to_string(),
                prompt,
                parent_id: None,
                model,
            });

            dialog.close();
        });
    }

    dialog.present();
}
