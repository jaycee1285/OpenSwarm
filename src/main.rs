mod agent;
mod app;
mod config;
mod ipc;
mod persistence;
mod supervisor;
mod ui;

use gtk4::prelude::*;

const APP_ID: &str = "dev.openswarm.app";

fn main() {
    if let Some(mode) = parse_mode() {
        match mode {
            RunMode::Supervisor => {
                let socket = ipc::socket_path();
                if let Err(e) = supervisor::server::run(&socket) {
                    eprintln!("Supervisor error: {e}");
                }
                return;
            }
            RunMode::Ui => {}
        }
    }

    let app = gtk4::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(|app| {
        ui::window::build(app);
    });

    app.run();
}

enum RunMode {
    Supervisor,
    Ui,
}

fn parse_mode() -> Option<RunMode> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--supervisor") {
        return Some(RunMode::Supervisor);
    }
    Some(RunMode::Ui)
}
