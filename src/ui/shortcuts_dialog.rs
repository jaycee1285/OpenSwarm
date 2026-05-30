use gtk4 as gtk;
use gtk4::prelude::*;

const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+N", "Spawn new agent"),
    ("Ctrl+W", "Kill selected agent"),
    ("Ctrl+R", "Resume selected agent"),
    ("Ctrl+Shift+W", "Remove selected session"),
    ("Ctrl+Tab", "Next agent"),
    ("Ctrl+Shift+Tab", "Previous agent"),
    ("Ctrl+1–9", "Select agent by position"),
    ("Ctrl+Left", "Focus left panel"),
    ("Ctrl+Up", "Focus terminal"),
    ("Ctrl+Right", "Toggle dashboard panel"),
    ("Ctrl+U", "Refresh usage status"),
    ("Ctrl+Shift+C", "Copy selection"),
    ("Ctrl+Shift+V", "Paste from clipboard"),
    ("Ctrl+H / Ctrl+K", "Show this help"),
];

pub fn show(parent: &gtk::ApplicationWindow) {
    let dialog = gtk::Window::builder()
        .title("Keyboard Shortcuts")
        .modal(true)
        .transient_for(parent)
        .default_width(340)
        .resizable(false)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(20);
    content.set_margin_end(20);
    dialog.set_child(Some(&content));

    let heading = gtk::Label::new(Some("Shortcuts"));
    heading.add_css_class("heading");
    heading.set_margin_bottom(8);
    content.append(&heading);

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(16);
    content.append(&grid);

    for (i, (key, desc)) in SHORTCUTS.iter().enumerate() {
        let key_label = gtk::Label::new(Some(key));
        key_label.set_halign(gtk::Align::Start);
        key_label.add_css_class("dim-label");
        key_label.set_xalign(0.0);

        let desc_label = gtk::Label::new(Some(desc));
        desc_label.set_halign(gtk::Align::Start);
        desc_label.set_xalign(0.0);

        grid.attach(&key_label, 0, i as i32, 1, 1);
        grid.attach(&desc_label, 1, i as i32, 1, 1);
    }

    let close_btn = gtk::Button::with_label("Close");
    close_btn.set_halign(gtk::Align::End);
    close_btn.set_margin_top(12);
    content.append(&close_btn);

    {
        let dialog = dialog.clone();
        close_btn.connect_clicked(move |_| {
            dialog.close();
        });
    }

    dialog.present();
}
