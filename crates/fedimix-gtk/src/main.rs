mod bridge;
mod cli;
mod demo_drive;
mod format;
mod operations;
mod present;
mod ui;
mod window;

use adw::prelude::*;
use fedimix_core::DemoBackend;

const APP_ID: &str = "io.sirion.Fedimix";

fn load_script(path: &std::path::Path) -> Result<Vec<demo_drive::Step>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    demo_drive::parse(&src).map_err(|e| format!("{}: {e}", path.display()))
}

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

    let drive = match args.drive.as_deref().map(load_script).transpose() {
        Ok(steps) => steps,
        Err(e) => {
            eprintln!("fedimix: {e}");
            return gtk::glib::ExitCode::from(2);
        }
    };

    let app = adw::Application::builder().application_id(APP_ID).build();
    let bridge = bridge::CoreBridge::start(DemoBackend::new(args.scenario));
    app.connect_activate(move |app| {
        let window = window::FedimixWindow::new(app, bridge.clone(), args.page);
        window.present();
        if let Some(steps) = drive.clone() {
            demo_drive::start(window.drive_env(app), steps);
        }
    });

    app.run_with_args::<&str>(&[])
}
