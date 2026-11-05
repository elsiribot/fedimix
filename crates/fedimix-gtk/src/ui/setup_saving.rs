//! Saving setup dialog: pick a detected hardware wallet, or this computer.

use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::DeviceInfo;
use gtk::glib;

use crate::present;
use crate::ui::app_state::AppContext;

pub fn open(ctx: &Rc<AppContext>) {
    open_for(ctx, None);
}

/// "Set up again" for the stored account, whose wallet did not open: only its
/// device can succeed, so only its device is offered, by name.
pub fn open_again(ctx: &Rc<AppContext>, stored: DeviceInfo) {
    open_for(ctx, Some(stored));
}

fn open_for(ctx: &Rc<AppContext>, stored: Option<DeviceInfo>) {
    let title = present::setup_title(stored.as_ref());
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
    // Why a hardware wallet may be missing from the list.
    let notes = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .visible(false)
        .build();
    notes.add_css_class("dim-label");
    let list_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    list_box.append(&notes);
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
    let empty_title = match &stored {
        Some(device) => format!("Connect your {} and unlock it", device.name),
        None => "Connect your device and unlock it".to_string(),
    };
    let empty = adw::StatusPage::builder()
        .icon_name("media-removable-symbolic")
        .title(empty_title)
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
        .title(title)
        .content_width(420)
        .child(&toolbar)
        .build();

    let view = View {
        list: list.downgrade(),
        pages: pages.downgrade(),
        dialog: dialog.downgrade(),
        error: error.downgrade(),
        notes: notes.downgrade(),
        stored,
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
    notes: glib::WeakRef<gtk::Label>,
    /// Set for "Set up again": the stored account's device.
    stored: Option<DeviceInfo>,
}

fn scan(ctx: &Rc<AppContext>, view: &View) {
    let core = ctx.bridge.core();
    let shown = ctx.clone();
    let view = view.clone();
    ctx.run(async move { core.detect_devices().await }, move |listing| {
        let (Some(list), Some(pages)) = (view.list.upgrade(), view.pages.upgrade()) else {
            return;
        };
        if let Some(label) = view.notes.upgrade() {
            let note = present::setup_note(&listing);
            label.set_label(note.unwrap_or(""));
            label.set_visible(note.is_some());
        }
        let devices = present::setup_choices(&listing.devices, view.stored.as_ref());
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
    // A device names itself, so its name is not markup.
    let row = adw::ActionRow::builder()
        .use_markup(false)
        .title(&device.name)
        .subtitle(present::setup_subtitle(device))
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
    // The whole row picks the device, as its button does.
    row.set_activatable_widget(Some(&button));
    row
}
