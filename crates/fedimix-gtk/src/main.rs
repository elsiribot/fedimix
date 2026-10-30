#![allow(dead_code)]
mod bridge;
mod cli;
mod format;
mod present;
mod ui;
mod window;

use adw::prelude::*;
use fedimix_core::DemoBackend;

const APP_ID: &str = "io.sirion.Fedimix";

fn main() -> gtk::glib::ExitCode {
    // Parse before GApplication runs: it would reject our custom flags.
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("fedimix: {e}");
            return gtk::glib::ExitCode::from(2);
        }
    };
    if !args.demo {
        eprintln!("Fedimix currently only runs with --demo");
        return gtk::glib::ExitCode::from(2);
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    let bridge = bridge::CoreBridge::start(DemoBackend::new(args.scenario));
    app.connect_activate(move |app| {
        window::FedimixWindow::new(app, bridge.clone()).present();
    });

    app.run_with_args::<&str>(&[])
}
