//! Saving setup dialog: pick a detected hardware wallet to set up.

use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::DeviceInfo;
use gtk::glib;

use crate::present;
use crate::ui::app_state::AppContext;

pub fn open(ctx: &Rc<AppContext>) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .build();
    list.add_css_class("boxed-list");
    let error = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .visible(false)
        .build();
    error.add_css_class("error");
    let list_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    list_box.append(&list);
    list_box.append(&error);
    let list_clamp = adw::Clamp::builder()
        .maximum_size(420)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .child(&list_box)
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

    let view = View {
        list: list.downgrade(),
        pages: pages.downgrade(),
        dialog: dialog.downgrade(),
        error: error.downgrade(),
    };
    retry.connect_clicked({
        let ctx = ctx.clone();
        let view = view.clone();
        move |_| scan(&ctx, &view)
    });
    scan(ctx, &view);
    dialog.present(Some(&ctx.nav));
}

/// Weak references: the closures live inside the dialog they point to.
#[derive(Clone)]
struct View {
    list: glib::WeakRef<gtk::ListBox>,
    pages: glib::WeakRef<gtk::Stack>,
    dialog: glib::WeakRef<adw::Dialog>,
    error: glib::WeakRef<gtk::Label>,
}

fn scan(ctx: &Rc<AppContext>, view: &View) {
    let core = ctx.bridge.core();
    let shown = ctx.clone();
    let view = view.clone();
    // The listing's `problem` is Task 8's to show.
    ctx.run(async move { core.detect_devices().await }, move |listing| {
        let devices = listing.devices;
        let (Some(list), Some(pages)) = (view.list.upgrade(), view.pages.upgrade()) else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        for device in &devices {
            list.append(&device_row(&shown, &view, device));
        }
        pages.set_visible_child_name(if devices.is_empty() { "empty" } else { "list" });
    });
}

fn device_row(ctx: &Rc<AppContext>, view: &View, device: &DeviceInfo) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&device.name)
        .subtitle(if device.connected {
            "Connected"
        } else {
            "Not connected"
        })
        .build();
    let spinner = gtk::Spinner::builder().visible(false).build();
    row.add_suffix(&spinner);
    let button = gtk::Button::with_label("Set up");
    button.set_valign(gtk::Align::Center);
    button.add_css_class("suggested-action");
    button.set_sensitive(device.connected);
    button.connect_clicked({
        let ctx = ctx.clone();
        let view = view.clone();
        let id = device.id.clone();
        let spinner = spinner.downgrade();
        move |button| {
            // One setup at a time: the button stays off until it resolves.
            button.set_sensitive(false);
            if let Some(spinner) = spinner.upgrade() {
                spinner.set_visible(true);
                spinner.set_spinning(true);
            }
            if let Some(error) = view.error.upgrade() {
                error.set_visible(false);
            }
            let core = ctx.bridge.core();
            let id = id.clone();
            let button = button.downgrade();
            let spinner = spinner.clone();
            let ctx2 = ctx.clone();
            let view = view.clone();
            ctx.bridge
                .call(async move { core.setup_saving(id).await }, move |res| {
                    if let Some(spinner) = spinner.upgrade() {
                        spinner.set_visible(false);
                        spinner.set_spinning(false);
                    }
                    match res {
                        Ok(()) => {
                            if let Some(dialog) = view.dialog.upgrade() {
                                dialog.close();
                            }
                            ctx2.toast("Saving is set up");
                            ctx2.refresh();
                        }
                        Err(e) => {
                            if let Some(button) = button.upgrade() {
                                button.set_sensitive(true);
                            }
                            let message = present::user_message(&e);
                            match view.error.upgrade() {
                                Some(error) => {
                                    error.set_label(&message);
                                    error.set_visible(true);
                                }
                                None => ctx2.toast(&message),
                            }
                        }
                    }
                });
        }
    });
    row.add_suffix(&button);
    row
}
