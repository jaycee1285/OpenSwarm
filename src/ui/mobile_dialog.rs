//! Mobile connection dialog for configuring WebSocket remote access.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::app::AppState;
use crate::config::load_config;
use crate::ipc::proto::ClientMessage;

/// Mobile connection state tracked by the UI.
pub struct MobileState {
    pub enabled: Cell<bool>,
    pub connected_peers: RefCell<Vec<String>>,
    /// Button widget for reactive state styling
    button: RefCell<Option<gtk::Button>>,
}

impl Default for MobileState {
    fn default() -> Self {
        let config = load_config();
        Self {
            enabled: Cell::new(config.ws_enabled),
            connected_peers: RefCell::new(Vec::new()),
            button: RefCell::new(None),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MobileButtonState {
    Off,
    Listening,
    Connected,
}

impl MobileState {
    fn button_state(&self) -> MobileButtonState {
        if !self.enabled.get() {
            MobileButtonState::Off
        } else if self.connected_peers.borrow().is_empty() {
            MobileButtonState::Listening
        } else {
            MobileButtonState::Connected
        }
    }

    /// Update the mobile button styling to reflect current connection state.
    pub fn update_button(&self) {
        if let Some(button) = self.button.borrow().as_ref() {
            button.remove_css_class("mobile-off");
            button.remove_css_class("mobile-listening");
            button.remove_css_class("mobile-connected");

            match self.button_state() {
                MobileButtonState::Off => {
                    button.add_css_class("mobile-off");
                    button.set_tooltip_text(Some("Mobile Connection: Off"));
                }
                MobileButtonState::Listening => {
                    button.add_css_class("mobile-listening");
                    button.set_tooltip_text(Some("Mobile Connection: Listening"));
                }
                MobileButtonState::Connected => {
                    button.add_css_class("mobile-connected");
                    button.set_tooltip_text(Some("Mobile Connection: Connected"));
                }
            }
        }
    }
}

/// Show the mobile connection configuration dialog.
pub fn show(parent: &gtk::ApplicationWindow, state: &Rc<AppState>, mobile_state: &Rc<MobileState>) {
    let dialog = gtk::Window::builder()
        .title("Mobile Connection")
        .modal(true)
        .transient_for(parent)
        .default_width(360)
        .default_height(200)
        .resizable(false)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    dialog.set_child(Some(&content));

    // Load current config
    let config = load_config();

    // --- Enable toggle ---
    let enable_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let enable_label = gtk::Label::new(Some("Enable Remote Access"));
    enable_label.set_hexpand(true);
    enable_label.set_halign(gtk::Align::Start);
    let enable_switch = gtk::Switch::new();
    enable_switch.set_active(config.ws_enabled);
    enable_box.append(&enable_label);
    enable_box.append(&enable_switch);
    content.append(&enable_box);

    // --- Password ---
    let password_label = gtk::Label::new(Some("Password"));
    password_label.set_halign(gtk::Align::Start);
    password_label.set_margin_top(8);
    content.append(&password_label);

    let password_entry = gtk::PasswordEntry::new();
    password_entry.set_show_peek_icon(true);
    password_entry.set_text(&config.ws_password);
    content.append(&password_entry);

    // --- Connected peers ---
    let peers = mobile_state.connected_peers.borrow();
    if !peers.is_empty() {
        let peers_label = gtk::Label::new(Some("Connected Devices"));
        peers_label.set_halign(gtk::Align::Start);
        peers_label.set_margin_top(12);
        peers_label.add_css_class("heading");
        content.append(&peers_label);

        for peer in peers.iter() {
            let peer_label = gtk::Label::new(Some(peer));
            peer_label.set_halign(gtk::Align::Start);
            peer_label.add_css_class("dim-label");
            content.append(&peer_label);
        }
    }
    drop(peers);

    let token_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    token_box.set_margin_top(12);
    let token_label = gtk::Label::new(Some("Token"));
    token_label.set_width_chars(6);
    token_label.set_halign(gtk::Align::Start);
    token_label.add_css_class("dim-label");
    let token_value = gtk::Label::new(Some(&config.ws_password));
    token_value.set_selectable(true);
    token_value.set_halign(gtk::Align::Start);
    token_value.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    token_box.append(&token_label);
    token_box.append(&token_value);
    content.append(&token_box);

    // --- Buttons ---
    let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    button_box.set_halign(gtk::Align::End);
    button_box.set_margin_top(12);
    let cancel_btn = gtk::Button::with_label("Cancel");
    let save_btn = gtk::Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    button_box.append(&cancel_btn);
    button_box.append(&save_btn);
    content.append(&button_box);

    // --- Cancel ---
    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            dialog.close();
        });
    }

    // --- Save ---
    {
        let dialog = dialog.clone();
        let state = state.clone();
        let mobile_state = mobile_state.clone();
        let enable_switch = enable_switch.clone();
        let password_entry = password_entry.clone();

        save_btn.connect_clicked(move |_| {
            let enabled = enable_switch.is_active();
            let password = password_entry.text().to_string();

            // Update local mobile state
            mobile_state.enabled.set(enabled);

            // Send config to supervisor
            state.ipc.send(&ClientMessage::SetWsConfig {
                enabled,
                password,
            });

            dialog.close();
        });
    }

    dialog.present();
}

/// Create a mobile connection button with status overlay.
pub fn create_button(
    state: &Rc<AppState>,
    mobile_state: &Rc<MobileState>,
    parent_window: &gtk::ApplicationWindow,
) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.set_tooltip_text(Some("Mobile Connection"));
    btn.set_has_frame(true);

    btn.add_css_class("mobile-button");
    let icon = gtk::Image::from_icon_name("phone-symbolic");
    icon.set_pixel_size(18);
    btn.set_child(Some(&icon));

    // Store button in MobileState for reactive updates
    *mobile_state.button.borrow_mut() = Some(btn.clone());

    // Initial style update
    mobile_state.update_button();

    // Connect click handler
    {
        let state = state.clone();
        let mobile_state = mobile_state.clone();
        let parent = parent_window.clone();
        btn.connect_clicked(move |_| {
            show(&parent, &state, &mobile_state);
        });
    }

    btn
}
