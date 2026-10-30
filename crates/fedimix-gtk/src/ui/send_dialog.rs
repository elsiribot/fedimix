//! Send dialog: recipient and amount, then a review page with Confirm.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::{
    FedimixError, OperationId, OperationState, SendAmount, SendPreview, SendRequest, WalletKind,
};
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
    let max = gtk::Button::with_label("Max");
    max.set_valign(gtk::Align::Center);
    max.add_css_class("flat");
    max.connect_clicked(glib::clone!(
        #[weak]
        amount,
        move |_| amount.set_text("max")
    ));
    amount.add_suffix(&max);
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
            amount.set_visible(!present::destination_has_amount(&d));
            let a = amount.text();
            set_error(&error, present::send_form_error(wallet, &d, &a).as_deref());
            review.set_sensitive(present::send_amount(wallet, &d, &a).is_ok());
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
            let amt = match present::send_amount(wallet, &d, &amount.text()) {
                Ok(a) => a,
                Err(e) => {
                    set_error(&error, Some(&e));
                    return;
                }
            };
            set_error(&error, None);
            button.set_sensitive(false);
            let request = SendRequest {
                wallet,
                destination: d,
                amount: amt,
                fee_rate_sat_vb: None,
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
                                nav.push(&review_page(&ctx, &dialog, &nav, &error, preview));
                            }
                            Err(e) => set_error(&error, Some(&present::user_message(&e))),
                        }
                    }
                ),
            );
        }
    ));

    if let Some(request) = prefill {
        dest.set_text(&request.destination);
        if let SendAmount::Exact(a) = request.amount {
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

/// The running part of a review: what confirming locks, and how it ends.
///
/// Closures hold this strongly only through the operation watcher, which the
/// context drops at the final state; the dialog cannot close before that, so
/// the outcome always finds its way here.
struct Progress {
    dialog: adw::Dialog,
    nav: adw::NavigationView,
    page: adw::NavigationPage,
    form_error: gtk::Label,
    error: gtk::Label,
    caption: gtk::Label,
    spinner: gtk::Spinner,
    confirm: gtk::Button,
    cancel: gtk::Button,
    device: String,
    /// Set while confirming, so a second activation does nothing.
    sending: Rc<Cell<bool>>,
    /// The preview, until it is confirmed.
    preview_id: RefCell<Option<String>>,
    operation: RefCell<Option<OperationId>>,
}

impl Progress {
    /// Back and Escape would drop the outcome, so both are off while sending.
    fn set_busy(&self, busy: bool) {
        self.dialog.set_can_close(!busy);
        self.page.set_can_pop(!busy);
        self.sending.set(busy);
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
        self.confirm.set_sensitive(!busy);
        if !busy {
            self.caption.set_visible(false);
            self.cancel.set_visible(false);
        }
    }

    fn back_to_form(&self, message: &str) {
        self.set_busy(false);
        self.nav.pop();
        set_error(&self.form_error, Some(message));
    }

    fn on_confirm_reply(
        self: Rc<Self>,
        ctx: &Rc<AppContext>,
        preview_id: String,
        res: Result<OperationId, FedimixError>,
    ) {
        match res {
            Ok(id) => {
                *self.operation.borrow_mut() = Some(id.clone());
                let ctx2 = ctx.clone();
                ctx.watch_operation(id, move |state| self.on_state(&ctx2, state));
            }
            Err(e @ FedimixError::PreviewExpired) => {
                self.back_to_form(&present::user_message(&e));
            }
            Err(e) => {
                *self.preview_id.borrow_mut() = Some(preview_id);
                self.set_busy(false);
                set_error(&self.error, Some(&present::user_message(&e)));
            }
        }
    }

    fn on_state(&self, ctx: &Rc<AppContext>, state: &OperationState) {
        self.caption
            .set_label(&present::operation_caption(state, &self.device));
        self.caption.set_visible(true);
        self.cancel
            .set_visible(present::operation_cancellable(state));
        match state {
            OperationState::Done => {
                self.set_busy(false);
                self.dialog.close();
                ctx.toast_now("Payment sent");
            }
            OperationState::Failed { .. } | OperationState::Cancelled => {
                self.back_to_form(&present::operation_caption(state, &self.device));
            }
            _ => {}
        }
    }

    /// Tell the core a preview the user walked away from is not needed.
    fn discard(&self, ctx: &Rc<AppContext>) {
        if let Some(id) = self.preview_id.borrow_mut().take() {
            let core = ctx.bridge.core();
            ctx.bridge
                .call(async move { core.discard_preview(id).await }, |_| {});
        }
    }
}

fn review_page(
    ctx: &Rc<AppContext>,
    dialog: &adw::Dialog,
    nav: &adw::NavigationView,
    form_error: &gtk::Label,
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
    let label = gtk::Label::builder()
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let spinner = gtk::Spinner::builder().visible(false).build();
    let confirm = gtk::Button::builder()
        .child(&button_content(&label, &spinner))
        .halign(gtk::Align::Center)
        .build();
    confirm.add_css_class("pill");
    confirm.add_css_class("suggested-action");
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("pill");
    cancel.set_halign(gtk::Align::Center);
    cancel.set_visible(false);
    let state = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .visible(false)
        .build();
    state.add_css_class("dim-label");

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
    let device = ctx.latest().and_then(|s| s.saving.device);
    apply_device(device.as_ref());
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

    body.append(&error);
    body.append(&state);
    body.append(&confirm);
    body.append(&cancel);
    let review = page("Review", &body);

    let progress = Rc::new(Progress {
        dialog: dialog.clone(),
        nav: nav.clone(),
        page: review.clone(),
        form_error: form_error.clone(),
        error: error.clone(),
        caption: state,
        spinner,
        confirm: confirm.clone(),
        cancel: cancel.clone(),
        device: device.map_or("your device".to_string(), |d| d.name),
        sending,
        preview_id: RefCell::new(Some(preview.id.clone())),
        operation: RefCell::new(None),
    });
    let weak = Rc::downgrade(&progress);

    confirm.connect_clicked({
        let ctx = ctx.clone();
        let weak = weak.clone();
        move |_| {
            let Some(progress) = weak.upgrade() else {
                return;
            };
            let Some(id) = progress.preview_id.borrow_mut().take() else {
                return;
            };
            progress.set_busy(true);
            set_error(&progress.error, None);
            let core = ctx.bridge.core();
            let ctx2 = ctx.clone();
            let id2 = id.clone();
            ctx.bridge
                .call(async move { core.confirm_send(id2).await }, move |res| {
                    progress.on_confirm_reply(&ctx2, id, res)
                });
        }
    });
    cancel.connect_clicked({
        let ctx = ctx.clone();
        let weak = weak.clone();
        move |button| {
            let Some(id) = weak.upgrade().and_then(|p| p.operation.borrow().clone()) else {
                return;
            };
            button.set_sensitive(false);
            let core = ctx.bridge.core();
            // Too late or already over: the operation's own state says so.
            ctx.bridge
                .call(async move { core.cancel_operation(id).await }, |_| {});
        }
    });
    // Leaving the review for good, by Back or by closing the dialog.
    review.connect_hidden({
        let ctx = ctx.clone();
        let weak = weak.clone();
        move |_| {
            if let Some(progress) = weak.upgrade() {
                progress.discard(&ctx);
            }
        }
    });
    dialog.connect_closed({
        let ctx = ctx.clone();
        move |_| {
            if let Some(progress) = weak.upgrade() {
                progress.discard(&ctx);
            }
        }
    });
    // The page owns the state; see `SavingTab::new`.
    review.connect_destroy(move |_| {
        let _keep_alive = &progress;
    });
    review
}
