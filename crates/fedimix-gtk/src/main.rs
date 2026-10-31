mod bridge;
mod cli;
mod demo_drive;
mod format;
mod operations;
mod present;
mod refresh;
mod ui;
mod window;

use std::sync::Arc;

use adw::prelude::*;
use fedimix_core::{Backend, CoreConfig, DemoBackend, DemoTiming};

const APP_ID: &str = "io.sirion.Fedimix";

fn load_script(path: &std::path::Path) -> Result<Vec<demo_drive::Step>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    demo_drive::parse(&src).map_err(|e| format!("{}: {e}", path.display()))
}

/// The folder for this run, or why there is none.
fn data_dir(args: &cli::Args) -> Result<std::path::PathBuf, String> {
    if let Some(dir) = &args.data_dir {
        return Ok(dir.clone());
    }
    let xdg = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    cli::default_data_dir(args.network, xdg.as_deref(), home.as_deref())
        .ok_or_else(|| "no home folder to keep data in; pass --data-dir".to_string())
}

/// Say why the wallet cannot start, in a dialog over an empty window, and
/// return 1 once it is closed. Nothing else opens.
fn show_start_failure(sentence: &str, drive: Option<Vec<demo_drive::Step>>) -> gtk::glib::ExitCode {
    eprintln!("fedimix: {sentence}");
    let sentence = sentence.to_string();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Fedimix")
            .default_width(360)
            .default_height(240)
            .build();
        window.present();
        let quit = gtk::gio::ActionEntry::builder("quit")
            .activate(|app: &adw::Application, _, _| app.quit())
            .build();
        app.add_action_entries([quit]);
        let dialog = adw::AlertDialog::new(Some("Fedimix cannot start"), Some(&sentence));
        dialog.add_response("close", "Close");
        // A drive ends through `quit`, so its last line cannot race the close.
        if drive.is_none() {
            dialog.connect_closed({
                let app = app.clone();
                move |_| app.quit()
            });
        }
        dialog.present(Some(&window));
        if let Some(steps) = drive.clone() {
            demo_drive::start(
                demo_drive::Env {
                    app: app.clone(),
                    window,
                    stack: adw::ViewStack::new(),
                },
                steps,
            );
        }
    });
    app.run_with_args::<&str>(&[]);
    gtk::glib::ExitCode::from(1)
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

    let drive = match args.drive.as_deref().map(load_script).transpose() {
        Ok(steps) => steps,
        Err(e) => {
            eprintln!("fedimix: {e}");
            return gtk::glib::ExitCode::from(2);
        }
    };

    let runtime = bridge::start_runtime();
    let (backend, send_lock_timeout): (Arc<dyn Backend>, _) = if args.demo {
        let mut timing = DemoTiming::for_scenario(args.scenario);
        if let Some(ms) = args.demo_step_ms {
            timing.step = std::time::Duration::from_millis(ms);
        }
        (
            DemoBackend::with_timing(args.scenario, timing),
            timing
                .send_lock_timeout
                .unwrap_or(present::SEND_LOCK_TIMEOUT),
        )
    } else {
        let dir = match data_dir(&args) {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("fedimix: {e}");
                return gtk::glib::ExitCode::from(2);
            }
        };
        let config = CoreConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            network: args.network,
        };
        match bridge::open_real(&runtime, config) {
            Ok(backend) => (backend, present::SEND_LOCK_TIMEOUT),
            Err(e) => return show_start_failure(&present::user_message(&e), drive),
        }
    };

    // Two networks can run side by side, and a second launch on one folder
    // must report the lock, so the real app never hands over to a first one.
    let flags = if args.demo {
        gtk::gio::ApplicationFlags::empty()
    } else {
        gtk::gio::ApplicationFlags::NON_UNIQUE
    };
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();
    let bridge = bridge::CoreBridge::on(runtime, backend);
    // One window for the app's lifetime: a second activation re-presents it.
    let window = std::cell::OnceCell::<window::FedimixWindow>::new();
    app.connect_activate(move |app| {
        if let Some(window) = window.get() {
            window.present();
            return;
        }
        let new = window::FedimixWindow::new(app, bridge.clone(), args.page, send_lock_timeout);
        new.present();
        if let Some(steps) = drive.clone() {
            demo_drive::start(new.drive_env(app), steps);
        }
        let _ = window.set(new);
    });

    let code = app.run_with_args::<&str>(&[]);
    if demo_drive::failed() {
        return gtk::glib::ExitCode::from(1);
    }
    code
}
