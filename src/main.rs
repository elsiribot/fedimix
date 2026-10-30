mod window;

use adw::prelude::*;

const APP_ID: &str = "io.sirion.Fedimix";

fn main() -> gtk::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_activate(|app| {
        window::FedimixWindow::new(app).present();
    });

    app.run()
}
