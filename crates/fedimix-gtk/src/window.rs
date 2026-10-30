use adw::prelude::*;

/// Main application window.
pub struct FedimixWindow {
    window: adw::ApplicationWindow,
}

impl FedimixWindow {
    pub fn new(app: &adw::Application) -> Self {
        let status_page = adw::StatusPage::builder()
            .title("Fedimix")
            .description("Fedimint-based bitcoin mixer")
            .icon_name("security-high-symbolic")
            .vexpand(true)
            .build();

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&adw::HeaderBar::new());
        toolbar_view.set_content(Some(&status_page));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Fedimix")
            .default_width(800)
            .default_height(600)
            .content(&toolbar_view)
            .build();

        Self { window }
    }

    pub fn present(&self) {
        self.window.present();
    }
}
