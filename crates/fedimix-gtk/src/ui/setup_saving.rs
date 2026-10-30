//! Saving setup dialog: pick a detected hardware wallet to set up.

use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::DeviceInfo;

use crate::ui::app_state::AppContext;

pub fn open(ctx: &Rc<AppContext>) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .build();
    list.add_css_class("boxed-list");
    let list_clamp = adw::Clamp::builder()
        .maximum_size(420)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .child(&list)
        .build();

    let retry = gtk::Button::with_label("Retry");
    retry.add_css_class("pill");
    retry.set_halign(gtk::Align::Center);
    let empty = adw::StatusPage::builder()
        .icon_name("media-removable-symbolic")
        .title("Connect your device and unlock it")
        .child(&retry)
        .vexpand(true)
        .build();

    let pages = gtk::Stack::new();
    pages.add_named(&list_clamp, Some("list"));
    pages.add_named(&empty, Some("empty"));
    pages.set_visible_child_name("empty");

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&pages));
    let dialog = adw::Dialog::builder()
        .title("Set up Saving")
        .content_width(420)
        .child(&toolbar)
        .build();

    let scan = {
        let ctx = ctx.clone();
        let list = list.clone();
        let pages = pages.clone();
        let dialog = dialog.clone();
        move || {
            let core = ctx.bridge.core();
            ctx.run(async move { core.detect_devices().await }, {
                let ctx = ctx.clone();
                let list = list.clone();
                let pages = pages.clone();
                let dialog = dialog.clone();
                move |devices| {
                    while let Some(child) = list.first_child() {
                        list.remove(&child);
                    }
                    for device in &devices {
                        list.append(&device_row(&ctx, &dialog, device));
                    }
                    pages.set_visible_child_name(if devices.is_empty() { "empty" } else { "list" });
                }
            });
        }
    };
    retry.connect_clicked({
        let scan = scan.clone();
        move |_| scan()
    });
    scan();
    dialog.present(Some(&ctx.nav));
}

fn device_row(ctx: &Rc<AppContext>, dialog: &adw::Dialog, device: &DeviceInfo) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&device.name)
        .subtitle(if device.connected {
            "Connected"
        } else {
            "Not connected"
        })
        .build();
    let button = gtk::Button::with_label("Set up");
    button.set_valign(gtk::Align::Center);
    button.add_css_class("suggested-action");
    button.set_sensitive(device.connected);
    button.connect_clicked({
        let ctx = ctx.clone();
        let dialog = dialog.clone();
        let device = device.clone();
        move |button| {
            // One setup at a time: the button stays off until it resolves.
            button.set_sensitive(false);
            let core = ctx.bridge.core();
            let device = device.clone();
            let button = button.downgrade();
            let ctx2 = ctx.clone();
            let dialog = dialog.clone();
            ctx.bridge.call(
                async move { core.setup_saving(device).await },
                move |res| match res {
                    Ok(()) => {
                        dialog.close();
                        ctx2.toast("Saving is set up");
                        ctx2.refresh();
                    }
                    Err(e) => {
                        if let Some(button) = button.upgrade() {
                            button.set_sensitive(true);
                        }
                        ctx2.toast(&crate::present::user_message(&e));
                    }
                },
            );
        }
    });
    row.add_suffix(&button);
    row
}
