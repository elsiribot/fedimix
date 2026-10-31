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

/// Shows the signing device's state on the Confirm button.
type ApplyDevice = dyn Fn(Option<&fedimix_core::DeviceInfo>);

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
/// The widgets are held weakly: the review page owns this state (see
/// `SavingTab::new`), so a strong reference back would keep the page, the
/// dialog and the snapshot subscription alive for good. The operation watcher
/// outlives the dialog and holds this strongly until the final state, and the
/// outcome is reported by toast when the dialog is gone.
struct Progress {
    ctx: Rc<AppContext>,
    dialog: glib::WeakRef<adw::Dialog>,
    nav: glib::WeakRef<adw::NavigationView>,
    page: glib::WeakRef<adw::NavigationPage>,
    form_error: glib::WeakRef<gtk::Label>,
    error: glib::WeakRef<gtk::Label>,
    caption: glib::WeakRef<gtk::Label>,
    spinner: glib::WeakRef<gtk::Spinner>,
    confirm: glib::WeakRef<gtk::Button>,
    cancel: glib::WeakRef<gtk::Button>,
    device: String,
    /// Set while confirming, so a second activation does nothing.
    sending: Rc<Cell<bool>>,
    /// The review page or the dialog has been left; an outcome can only be
    /// a toast.
    closed: Cell<bool>,
    /// The preview, until it is confirmed.
    preview_id: RefCell<Option<String>>,
    operation: RefCell<Option<OperationId>>,
    /// Ends the lock if Broadcasting does not end (see `SEND_LOCK_TIMEOUT`).
    unlock_timer: RefCell<Option<glib::SourceId>>,
    /// Puts Confirm's label and sensitivity back to what the device says.
    show_device: Box<dyn Fn()>,
}

impl Progress {
    /// Back and Escape would drop the outcome, so both are off while sending.
    fn set_busy(&self, busy: bool) {
        self.set_closable(!busy);
        self.sending.set(busy);
        if let Some(spinner) = self.spinner.upgrade() {
            spinner.set_visible(busy);
            spinner.set_spinning(busy);
        }
        if let Some(confirm) = self.confirm.upgrade() {
            confirm.set_sensitive(!busy);
        }
        if !busy {
            if let Some(caption) = self.caption.upgrade() {
                caption.set_visible(false);
            }
            if let Some(cancel) = self.cancel.upgrade() {
                cancel.set_visible(false);
            }
            // After an error the device may have changed meanwhile.
            (self.show_device)();
        }
    }

    fn set_closable(&self, closable: bool) {
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.set_can_close(closable);
        }
        if let Some(page) = self.page.upgrade() {
            page.set_can_pop(closable);
        }
    }

    fn back_to_form(&self, message: &str) {
        self.set_busy(false);
        if let Some(nav) = self.nav.upgrade() {
            nav.pop();
        }
        if let Some(label) = self.form_error.upgrade() {
            set_error(&label, Some(message));
        }
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
                ctx.watch_operation(id, move |state| self.on_state(state));
            }
            // The core may have used the preview up, and Review makes a new one.
            Err(e) => {
                *self.preview_id.borrow_mut() = Some(preview_id);
                self.discard(ctx);
                self.back_to_form(&present::user_message(&e));
            }
        }
    }

    fn on_state(self: &Rc<Self>, state: &OperationState) {
        let text = present::operation_caption(state, &self.device);
        if let Some(caption) = self.caption.upgrade() {
            caption.set_label(&text);
            caption.set_visible(true);
        }
        if let Some(cancel) = self.cancel.upgrade() {
            cancel.set_visible(present::operation_cancellable(state));
        }
        if *state == OperationState::Broadcasting {
            self.arm_unlock();
        }
        if !state.is_final() {
            return;
        }
        if let Some(id) = self.unlock_timer.borrow_mut().take() {
            id.remove();
        }
        match state {
            OperationState::Done => {
                self.set_busy(false);
                // A dialog the user has left may be in use for something
                // else by now: leave it alone.
                if !self.closed.get()
                    && let Some(dialog) = self.dialog.upgrade()
                {
                    dialog.close();
                }
                self.ctx.toast_now("Payment sent");
            }
            _ if self.closed.get() => self.ctx.toast_now(&text),
            _ => self.back_to_form(&text),
        }
    }

    /// An operation that never ends must not lock the app: after
    /// `SEND_LOCK_TIMEOUT` in Broadcasting the dialog may be closed. It stays
    /// open until the user does, and the watcher reports the outcome either
    /// way.
    fn arm_unlock(self: &Rc<Self>) {
        if self.unlock_timer.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(self.ctx.send_lock_timeout(), move || {
            let Some(this) = weak.upgrade() else { return };
            this.unlock_timer.borrow_mut().take();
            this.set_closable(true);
            if let Some(caption) = this.caption.upgrade() {
                caption.set_label(present::SEND_UNLOCKED_CAPTION);
            }
        });
        *self.unlock_timer.borrow_mut() = Some(id);
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
    hero.set_opens_details(present::review_discloses(&preview));
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
    body.append(&caption(present::fee_label(wallet)));
    body.append(&long_text(&format::sats(preview.fee)));
    body.append(&caption("Total"));
    body.append(&long_text(&present::review_total(&preview)));

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
    let apply_device: Rc<ApplyDevice> = Rc::new({
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
    });
    let device = ctx.latest().and_then(|s| s.saving.device);
    apply_device(device.as_ref());
    ctx.on_snapshots({
        let alive = confirm.downgrade();
        let apply_device = apply_device.clone();
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
        ctx: ctx.clone(),
        dialog: dialog.downgrade(),
        nav: nav.downgrade(),
        page: review.downgrade(),
        form_error: form_error.downgrade(),
        error: error.downgrade(),
        caption: state.downgrade(),
        spinner: spinner.downgrade(),
        confirm: confirm.downgrade(),
        cancel: cancel.downgrade(),
        device: device.map_or("your device".to_string(), |d| d.name),
        sending,
        closed: Cell::new(false),
        preview_id: RefCell::new(Some(preview.id.clone())),
        operation: RefCell::new(None),
        unlock_timer: RefCell::new(None),
        show_device: Box::new({
            let ctx = ctx.clone();
            move || apply_device(ctx.latest().and_then(|s| s.saving.device).as_ref())
        }),
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
            if let Some(label) = progress.error.upgrade() {
                set_error(&label, None);
            }
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
                progress.closed.set(true);
                progress.discard(&ctx);
            }
        }
    });
    dialog.connect_closed({
        let ctx = ctx.clone();
        move |_| {
            if let Some(progress) = weak.upgrade() {
                progress.closed.set(true);
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
