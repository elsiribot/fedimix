//! Small shared widgets used by the Saving and Spending tabs.

use std::time::{SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use fedimix_core::WalletKind;

use crate::ui::app_state::AppContext;

/// Flat button showing the hero amount followed by a chevron.
#[derive(Clone)]
pub struct AmountButton {
    button: gtk::Button,
    label: gtk::Label,
}

impl AmountButton {
    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    pub fn set_amount(&self, text: &str) {
        self.label.set_label(text);
    }

    pub fn connect_clicked(&self, f: impl Fn() + 'static) {
        self.button.connect_clicked(move |_| f());
    }
}

/// `css` is the style class of the amount label, e.g. `hero-balance`.
pub fn amount_button(css: &str) -> AmountButton {
    let label = gtk::Label::builder()
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    label.add_css_class(css);
    let chevron = gtk::Label::new(Some("\u{203a}"));
    chevron.add_css_class(css);
    chevron.add_css_class("dim-label");
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    content.append(&label);
    content.append(&chevron);
    let button = gtk::Button::builder()
        .child(&content)
        .halign(gtk::Align::Center)
        .build();
    button.add_css_class("flat");
    AmountButton { button, label }
}

/// Centered vertical container for the hero block.
pub fn hero_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .margin_top(12)
        .margin_bottom(12)
        .build()
}

/// Centered `Send` / `Receive` pill buttons for `wallet`.
pub fn send_receive_row(ctx: &AppContext, _wallet: WalletKind) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .margin_top(6)
        .build();
    let send = gtk::Button::with_label("Send");
    send.add_css_class("pill");
    send.add_css_class("suggested-action");
    let receive = gtk::Button::with_label("Receive");
    receive.add_css_class("pill");
    // Task 9 replaces these with send_dialog::open / receive_dialog::open.
    for button in [&send, &receive] {
        let ctx = ctx.clone();
        button.connect_clicked(move |_| ctx.toast("Not available yet"));
    }
    row.append(&send);
    row.append(&receive);
    row
}

/// Current Unix time in seconds.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
