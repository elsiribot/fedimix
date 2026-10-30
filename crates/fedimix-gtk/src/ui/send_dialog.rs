//! Send dialog: recipient and amount, then a review page with Confirm.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::{SendPreview, SendRequest, WalletKind};
use gtk::glib;

use crate::format;
use crate::present;
use crate::ui::app_state::AppContext;
use crate::ui::widgets;

/// Open the Send dialog for `wallet`.
pub fn open(ctx: &Rc<AppContext>, wallet: WalletKind) {
    let (dialog, _) = build(ctx, wallet, None);
    dialog.present(Some(&ctx.nav));
}

/// Open the dialog with `request` filled in and advance to the review page.
/// Used by `--demo-page send-review`.
pub fn open_review(ctx: &Rc<AppContext>, request: SendRequest) {
    let (dialog, review) = build(ctx, request.wallet, Some(&request));
    dialog.present(Some(&ctx.nav));
    review.emit_clicked();
}

fn error_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .visible(false)
        .selectable(true)
        .build();
    label.add_css_class("error");
    label
}

fn set_error(label: &gtk::Label, text: Option<&str>) {
    label.set_label(text.unwrap_or(""));
    label.set_visible(text.is_some());
}

/// Selectable label that breaks long strings anywhere.
fn long_text(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::Char)
        .xalign(0.0)
        .selectable(true)
        .max_width_chars(40)
        .build()
}

fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label
}

fn page(title: &str, body: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    let clamp = adw::Clamp::builder()
        .maximum_size(500)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .child(body)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&clamp)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroll));
    adw::NavigationPage::new(&toolbar, title)
}

fn build(
    ctx: &Rc<AppContext>,
    wallet: WalletKind,
    prefill: Option<&SendRequest>,
) -> (adw::Dialog, gtk::Button) {
    let hint = match wallet {
        WalletKind::Spending => "Lightning invoice or on-chain address",
        WalletKind::Saving => "Bitcoin address",
    };
    let dest = adw::EntryRow::builder().title(hint).build();
    let amount = adw::EntryRow::builder().title("Amount (BTC)").build();
    amount.set_input_purpose(gtk::InputPurpose::Number);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    list.append(&dest);
    list.append(&amount);
    let error = error_label();
    let review = gtk::Button::with_label("Review");
    review.add_css_class("pill");
    review.add_css_class("suggested-action");
    review.set_halign(gtk::Align::Center);
    review.set_sensitive(false);

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    body.append(&list);
    body.append(&error);
    body.append(&review);
    let first = page("Send", &body);

    let nav = adw::NavigationView::new();
    nav.add(&first);
    let dialog = adw::Dialog::builder()
        .title("Send")
        .content_width(420)
        .content_height(460)
        .child(&nav)
        .build();

    // Holds only weak widget references, so it creates no reference cycle.
    let update: Rc<dyn Fn()> = Rc::new({
        let dest = dest.downgrade();
        let amount = amount.downgrade();
        let error = error.downgrade();
        let review = review.downgrade();
        move || {
            let (Some(dest), Some(amount), Some(error), Some(review)) = (
                dest.upgrade(),
                amount.upgrade(),
                error.upgrade(),
                review.upgrade(),
            ) else {
                return;
            };
            let d = dest.text();
            let requires = present::send_requires_amount(wallet, &d);
            amount.set_visible(requires);
            let a = amount.text();
            set_error(
                &error,
                present::send_form_error(&d, &a, requires).as_deref(),
            );
            review.set_sensitive(present::send_validation(&d, &a, requires).is_ok());
        }
    });
    for row in [&dest, &amount] {
        let update = update.clone();
        row.connect_changed(move |_| update());
    }

    review.connect_clicked(glib::clone!(
        #[strong]
        ctx,
        #[weak]
        dest,
        #[weak]
        amount,
        #[weak]
        error,
        #[weak]
        nav,
        #[weak]
        dialog,
        move |button| {
            let d = dest.text().trim().to_string();
            let amt = if present::send_requires_amount(wallet, &d) {
                match format::parse_btc(&amount.text()) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        set_error(&error, Some(&e));
                        return;
                    }
                }
            } else {
                None
            };
            set_error(&error, None);
            button.set_sensitive(false);
            let request = SendRequest {
                wallet,
                destination: d,
                amount: amt,
            };
            let core = ctx.bridge.core();
            ctx.bridge.call(
                async move { core.prepare_send(request).await },
                glib::clone!(
                    #[strong]
                    ctx,
                    #[weak]
                    button,
                    #[weak]
                    error,
                    #[weak]
                    nav,
                    #[weak]
                    dialog,
                    move |res| {
                        button.set_sensitive(true);
                        match res {
                            Ok(preview) => {
                                nav.push(&review_page(&ctx, &dialog, preview));
                            }
                            Err(e) => set_error(&error, Some(&present::send_error_message(&e))),
                        }
                    }
                ),
            );
        }
    ));

    if let Some(request) = prefill {
        dest.set_text(&request.destination);
        if let Some(a) = request.amount {
            amount.set_text(&format::btc_short(a));
        }
    }
    update();
    (dialog, review)
}

/// Box with an optional spinner followed by `label`.
fn button_content(label: &gtk::Label, spinner: &gtk::Spinner) -> gtk::Box {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    content.append(spinner);
    content.append(label);
    content
}

fn detail_lines(preview: &SendPreview) -> gtk::Box {
    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(4)
        .build();
    details.append(&caption("Spent from"));
    for input in &preview.inputs {
        let line = long_text(&format!(
            "{} \u{b7} {}",
            format::btc(input.amount),
            input.outpoint_or_note
        ));
        line.add_css_class("caption");
        details.append(&line);
    }
    if let Some(text) = present::change_text(&preview.change) {
        let line = long_text(&text);
        line.add_css_class("caption");
        details.append(&line);
    }
    details
}

fn review_page(
    ctx: &Rc<AppContext>,
    dialog: &adw::Dialog,
    preview: SendPreview,
) -> adw::NavigationPage {
    let wallet = preview.wallet;
    let hero = widgets::amount_button("title-1");
    hero.set_amount(&format::btc_exact(preview.amount));
    let revealer = gtk::Revealer::builder()
        .child(&detail_lines(&preview))
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();
    hero.connect_clicked({
        let revealer = revealer.downgrade();
        move || {
            if let Some(r) = revealer.upgrade() {
                r.set_reveal_child(!r.reveals_child());
            }
        }
    });

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    body.append(hero.widget());
    body.append(&revealer);
    body.append(&caption("To"));
    body.append(&long_text(&preview.destination));
    body.append(&caption("Network fee"));
    body.append(&long_text(&format::sats(preview.fee)));

    let error = error_label();
    let label = gtk::Label::new(None);
    let spinner = gtk::Spinner::builder().visible(false).build();
    let confirm = gtk::Button::builder()
        .child(&button_content(&label, &spinner))
        .halign(gtk::Align::Center)
        .build();
    confirm.add_css_class("pill");
    confirm.add_css_class("suggested-action");
    let sending = Rc::new(Cell::new(false));

    let apply_device = {
        let label = label.downgrade();
        let confirm = confirm.downgrade();
        let sending = sending.clone();
        move |device: Option<&fedimix_core::DeviceInfo>| {
            if sending.get() {
                return;
            }
            let (Some(label), Some(confirm)) = (label.upgrade(), confirm.upgrade()) else {
                return;
            };
            let (text, enabled) = present::confirm_button(wallet, device);
            label.set_label(&text);
            confirm.set_sensitive(enabled);
        }
    };
    apply_device(ctx.latest().and_then(|s| s.saving.device).as_ref());
    ctx.on_snapshots({
        let alive = confirm.downgrade();
        move |s| {
            if alive.upgrade().is_none() {
                return false;
            }
            apply_device(s.saving.device.as_ref());
            true
        }
    });

    confirm.connect_clicked(glib::clone!(
        #[strong]
        ctx,
        #[strong]
        sending,
        #[weak]
        spinner,
        #[weak]
        error,
        #[strong(rename_to = dialog)]
        dialog.downgrade(),
        move |button| {
            // Second activation while the call is in flight does nothing.
            if sending.replace(true) {
                return;
            }
            button.set_sensitive(false);
            set_error(&error, None);
            spinner.set_visible(true);
            spinner.start();
            let core = ctx.bridge.core();
            let id = preview.id.clone();
            let dialog = dialog.clone();
            ctx.bridge.call(
                async move { core.confirm_send(id).await },
                glib::clone!(
                    #[strong]
                    ctx,
                    #[strong]
                    sending,
                    #[weak]
                    button,
                    #[weak]
                    spinner,
                    #[weak]
                    error,
                    move |res| match res {
                        Ok(()) => {
                            // The button stays insensitive; the dialog goes away.
                            if let Some(dialog) = dialog.upgrade() {
                                dialog.close();
                            }
                            ctx.toast("Payment sent");
                            ctx.refresh();
                        }
                        Err(e) => {
                            sending.set(false);
                            spinner.stop();
                            spinner.set_visible(false);
                            button.set_sensitive(true);
                            set_error(&error, Some(&present::send_error_message(&e)));
                        }
                    }
                ),
            );
        }
    ));
    body.append(&error);
    body.append(&confirm);
    page("Review", &body)
}
