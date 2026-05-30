use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::app::AppState;
use crate::config;

pub fn show(parent: &gtk::ApplicationWindow, state: &Rc<AppState>) {
    let dialog = gtk::Window::builder()
        .title("Settings")
        .modal(true)
        .transient_for(parent)
        .default_width(360)
        .resizable(false)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    dialog.set_child(Some(&content));

    let heading = gtk::Label::new(Some("Terminal Scheme"));
    heading.set_halign(gtk::Align::Start);
    heading.add_css_class("heading");
    content.append(&heading);

    let options = config::TERMINAL_THEME_OPTIONS;
    let labels: Vec<&str> = options.iter().map(|option| option.label).collect();
    let list = gtk::StringList::new(&labels);
    let dropdown = gtk::DropDown::new(Some(list), None::<gtk::Expression>);

    let cfg = config::load_config();
    if let Some(index) = options.iter().position(|option| option.id == cfg.terminal_scheme) {
        dropdown.set_selected(index as u32);
    }
    content.append(&dropdown);

    let hint = gtk::Label::new(Some(
        "Syntax colors come from the selected tmTheme. The rest of the app follows your GTK theme.",
    ));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    hint.add_css_class("dim-label");
    content.append(&hint);

    let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    button_box.set_halign(gtk::Align::End);
    button_box.set_margin_top(8);

    let cancel_btn = gtk::Button::with_label("Cancel");
    let save_btn = gtk::Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    button_box.append(&cancel_btn);
    button_box.append(&save_btn);
    content.append(&button_box);

    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            dialog.close();
        });
    }

    {
        let dialog = dialog.clone();
        let dropdown = dropdown.clone();
        let state = state.clone();
        save_btn.connect_clicked(move |_| {
            let selected = dropdown.selected() as usize;
            let Some(option) = options.get(selected).copied() else {
                return;
            };

            let mut cfg = config::load_config();
            cfg.terminal_scheme = option.id.to_string();
            if let Err(err) = config::save_config(&cfg) {
                eprintln!("Failed to save terminal scheme: {err}");
                return;
            }

            let agents = state.agents.borrow();
            for entry in agents.iter() {
                config::apply_terminal_theme_by_id(&entry.terminal, option.id);
            }

            dialog.close();
        });
    }

    dialog.present();
}
