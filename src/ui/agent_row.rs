use std::cell::Cell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::agent::status::AgentStatus;
use crate::agent::types::AgentType;

/// Create an agent row for the left panel list.
/// Returns (ListBoxRow, Label) — the row and the status dot for later updates.
pub fn create(
    _id: u32,
    agent_type: AgentType,
    repo_name: &str,
    status: Rc<Cell<AgentStatus>>,
) -> (gtk::ListBoxRow, gtk::Label) {
    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row_box.set_margin_top(4);
    row_box.set_margin_bottom(4);
    row_box.set_margin_start(8);
    row_box.set_margin_end(8);

    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class("status-dot");
    dot.set_width_chars(1);
    dot.set_halign(gtk::Align::Center);
    dot.set_valign(gtk::Align::Center);
    super::window::apply_status_dot_class(&dot, status.get());
    row_box.append(&dot);

    // Agent type label
    let type_label = gtk::Label::new(Some(agent_type.label()));
    type_label.add_css_class("agent-type");
    row_box.append(&type_label);

    // Repo name label
    let repo_label = gtk::Label::new(Some(repo_name));
    repo_label.add_css_class("repo-name");
    repo_label.set_hexpand(true);
    repo_label.set_halign(gtk::Align::End);
    repo_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row_box.append(&repo_label);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_box));

    (row, dot)
}
