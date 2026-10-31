//! Receive dialog: Lightning invoice and on-chain address with QR.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::{Amount, ReceiveInfo, ReceiveMethod, ReceiveRequest, WalletKind, parse_btc};

use gtk::glib;

use crate::present::{self, user_message};
use crate::ui::app_state::AppContext;
use crate::ui::{qr, widgets};

/// How long a new invoice stays payable.
const INVOICE_EXPIRY_SECS: u32 = 3_600;

/// QR, selectable payload text and a Copy button.
#[derive(Clone)]
struct PayloadView {
    root: gtk::Box,
    picture: gtk::Picture,
    text: gtk::Label,
    copy: gtk::Button,
    expiry: gtk::Label,
    minimum: gtk::Label,
    status: gtk::Label,
    spinner: gtk::Spinner,
    retry: gtk::Button,
    content: gtk::Box,
}

/// Weak handles to a `PayloadView`'s widgets, for closures the view's own
/// widgets hold (a strong clone there is a reference cycle).
struct WeakPayloadView {
    root: glib::WeakRef<gtk::Box>,
    picture: glib::WeakRef<gtk::Picture>,
    text: glib::WeakRef<gtk::Label>,
    copy: glib::WeakRef<gtk::Button>,
    expiry: glib::WeakRef<gtk::Label>,
    minimum: glib::WeakRef<gtk::Label>,
    status: glib::WeakRef<gtk::Label>,
    spinner: glib::WeakRef<gtk::Spinner>,
    retry: glib::WeakRef<gtk::Button>,
    content: glib::WeakRef<gtk::Box>,
}

impl WeakPayloadView {
    fn upgrade(&self) -> Option<PayloadView> {
        Some(PayloadView {
            root: self.root.upgrade()?,
            picture: self.picture.upgrade()?,
            text: self.text.upgrade()?,
            copy: self.copy.upgrade()?,
            expiry: self.expiry.upgrade()?,
            minimum: self.minimum.upgrade()?,
            status: self.status.upgrade()?,
            spinner: self.spinner.upgrade()?,
            retry: self.retry.upgrade()?,
            content: self.content.upgrade()?,
        })
    }
}

impl PayloadView {
    fn downgrade(&self) -> WeakPayloadView {
        WeakPayloadView {
            root: self.root.downgrade(),
            picture: self.picture.downgrade(),
            text: self.text.downgrade(),
            copy: self.copy.downgrade(),
            expiry: self.expiry.downgrade(),
            minimum: self.minimum.downgrade(),
            status: self.status.downgrade(),
            spinner: self.spinner.downgrade(),
            retry: self.retry.downgrade(),
            content: self.content.downgrade(),
        }
    }

    fn new(toasts: &adw::ToastOverlay) -> Self {
        let picture = gtk::Picture::builder()
            .halign(gtk::Align::Center)
            .can_shrink(true)
            .build();
        let text = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::Char)
            .max_width_chars(40)
            .selectable(true)
            .xalign(0.0)
            .build();
        let copy = gtk::Button::with_label("Copy");
        copy.add_css_class("pill");
        copy.set_halign(gtk::Align::Center);
        let expiry = gtk::Label::builder().visible(false).xalign(0.0).build();
        expiry.add_css_class("dim-label");
        expiry.add_css_class("caption");
        let minimum = gtk::Label::builder()
            .visible(false)
            .wrap(true)
            .xalign(0.0)
            .build();
        minimum.add_css_class("dim-label");
        minimum.add_css_class("caption");
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();
        content.append(&picture);
        content.append(&text);
        content.append(&minimum);
        content.append(&expiry);
        content.append(&copy);
        let status = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .visible(false)
            .build();
        let spinner = gtk::Spinner::builder()
            .visible(false)
            .halign(gtk::Align::Center)
            .build();
        let retry = gtk::Button::with_label("Retry");
        retry.add_css_class("pill");
        retry.set_halign(gtk::Align::Center);
        retry.set_visible(false);
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        root.append(&spinner);
        root.append(&status);
        root.append(&retry);
        root.append(&content);

        copy.connect_clicked(glib::clone!(
            #[weak]
            text,
            #[weak]
            toasts,
            move |button| {
                button.clipboard().set_text(&text.label());
                toasts.add_toast(adw::Toast::new("Copied"));
            }
        ));
        Self {
            root,
            picture,
            text,
            copy,
            expiry,
            minimum,
            status,
            spinner,
            retry,
            content,
        }
    }

    fn show(&self, info: &ReceiveInfo) {
        self.set_loading(false);
        self.text.set_label(&info.payload);
        match qr::texture(&info.uri) {
            Ok(texture) => {
                self.picture.set_paintable(Some(&texture));
                self.picture
                    .set_size_request(texture.width(), texture.height());
                self.picture.set_visible(true);
            }
            Err(e) => {
                self.picture.set_visible(false);
                self.show_status(&format!("Could not draw QR code: {e}"));
            }
        }
        let expiry = info
            .expires_at
            .map(|at| present::expiry_line(at, widgets::now()));
        self.expiry.set_visible(expiry.is_some());
        self.expiry.set_label(expiry.as_deref().unwrap_or(""));
        let minimum = present::receive_minimum(info);
        self.minimum.set_visible(minimum.is_some());
        self.minimum.set_label(minimum.as_deref().unwrap_or(""));
        self.copy.set_sensitive(true);
        self.content.set_visible(true);
    }

    fn show_status(&self, text: &str) {
        self.status.set_label(text);
        self.status.set_visible(true);
    }

    fn clear(&self) {
        self.status.set_visible(false);
        self.retry.set_visible(false);
        self.content.set_visible(false);
    }

    fn set_loading(&self, loading: bool) {
        self.spinner.set_visible(loading);
        self.spinner.set_spinning(loading);
    }

    fn show_failure(&self, text: &str) {
        self.set_loading(false);
        self.show_status(text);
        self.retry.set_visible(true);
    }
}

fn scrolled(body: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::builder()
        .maximum_size(500)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .child(body)
        .build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&clamp)
        .build()
}

fn vbox() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build()
}

/// Fetch the deposit address into `view`.
fn load_address(ctx: &Rc<AppContext>, wallet: WalletKind, view: &PayloadView) {
    view.clear();
    view.set_loading(true);
    let core = ctx.bridge.core();
    let request = ReceiveRequest {
        wallet,
        method: ReceiveMethod::OnChain,
    };
    ctx.bridge.call(
        async move { core.receive(request).await },
        glib::clone!(
            #[strong]
            view,
            move |res| match res {
                Ok(info) => view.show(&info),
                Err(e) => {
                    view.show_failure(&format!("Could not get an address. {}", user_message(&e)));
                }
            }
        ),
    );
}

/// Retry reloads the address after a failure.
fn connect_retry(ctx: &Rc<AppContext>, wallet: WalletKind, view: &PayloadView) {
    // The retry button is part of `view`: hold the view weakly.
    let weak = view.downgrade();
    view.retry.connect_clicked(glib::clone!(
        #[strong]
        ctx,
        move |_| {
            if let Some(view) = weak.upgrade() {
                load_address(&ctx, wallet, &view);
            }
        }
    ));
}

fn lightning_page(ctx: &Rc<AppContext>, toasts: &adw::ToastOverlay) -> gtk::Box {
    let amount = adw::EntryRow::builder().title("Amount (BTC)").build();
    amount.set_input_purpose(gtk::InputPurpose::Number);
    let description = adw::EntryRow::builder()
        .title("Description (optional)")
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    list.append(&amount);
    list.append(&description);
    let create = gtk::Button::with_label("Create invoice");
    create.add_css_class("pill");
    create.add_css_class("suggested-action");
    create.set_halign(gtk::Align::Center);
    create.set_sensitive(false);
    let view = PayloadView::new(toasts);
    let page = vbox();
    page.append(&list);
    page.append(&create);
    page.append(&view.root);

    // Bumped by each request and edit; a reply for an older one is dropped.
    let generation = Rc::new(Cell::new(0u64));
    // An edited amount or description makes the shown invoice stale.
    let stale = glib::clone!(
        #[weak]
        create,
        #[weak]
        amount,
        #[strong]
        view,
        #[strong]
        generation,
        move |_: &adw::EntryRow| {
            generation.set(generation.get() + 1);
            view.clear();
            view.set_loading(false);
            create.set_sensitive(matches!(
                parse_btc(&amount.text()),
                Ok(a) if a != Amount::ZERO
            ));
        }
    );
    amount.connect_changed(stale.clone());
    description.connect_changed(stale);
    create.connect_clicked(glib::clone!(
        #[strong]
        ctx,
        #[weak]
        amount,
        #[weak]
        description,
        #[strong]
        view,
        #[strong]
        generation,
        move |button| {
            let Ok(sats) = parse_btc(&amount.text()) else {
                return;
            };
            let current = generation.get() + 1;
            generation.set(current);
            view.clear();
            view.set_loading(true);
            button.set_sensitive(false);
            let core = ctx.bridge.core();
            let request = ReceiveRequest {
                wallet: WalletKind::Spending,
                method: ReceiveMethod::Lightning {
                    amount: sats,
                    description: description.text().trim().to_string(),
                    expiry_secs: INVOICE_EXPIRY_SECS,
                },
            };
            ctx.bridge.call(
                async move { core.receive(request).await },
                glib::clone!(
                    #[weak]
                    button,
                    #[strong]
                    view,
                    #[strong]
                    generation,
                    move |res| {
                        if generation.get() != current {
                            return;
                        }
                        button.set_sensitive(true);
                        view.set_loading(false);
                        match res {
                            Ok(info) => view.show(&info),
                            Err(e) => view.show_status(&format!(
                                "Could not create invoice. {}",
                                user_message(&e)
                            )),
                        }
                    }
                ),
            );
        }
    ));
    page
}

/// Open the Receive dialog for `wallet`.
pub fn open(ctx: &Rc<AppContext>, wallet: WalletKind) {
    let toasts = adw::ToastOverlay::new();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let view = PayloadView::new(&toasts);

    match wallet {
        WalletKind::Spending => {
            let stack = gtk::Stack::new();
            let lightning = lightning_page(ctx, &toasts);
            stack.add_titled(&scrolled(&lightning), Some("lightning"), "Lightning");
            let body = vbox();
            body.append(&view.root);
            stack.add_titled(&scrolled(&body), Some("onchain"), "On-chain");
            // Addresses are finite: allocate one only for someone who looks.
            let requested = Cell::new(false);
            stack.connect_visible_child_name_notify(glib::clone!(
                #[strong]
                ctx,
                #[strong]
                view,
                move |stack| {
                    if stack.visible_child_name().as_deref() == Some("onchain")
                        && !requested.replace(true)
                    {
                        load_address(&ctx, wallet, &view);
                    }
                }
            ));
            let switcher = gtk::StackSwitcher::builder().stack(&stack).build();
            header.set_title_widget(Some(&switcher));
            toolbar.set_content(Some(&stack));
        }
        WalletKind::Saving => {
            header.set_title_widget(Some(&adw::WindowTitle::new("Receive", "")));
            let body = vbox();
            body.append(&view.root);
            let note = gtk::Label::builder()
                .label("Coins received this way are marked unmixed")
                .wrap(true)
                .xalign(0.0)
                .build();
            note.add_css_class("dim-label");
            note.add_css_class("caption");
            body.append(&note);
            toolbar.set_content(Some(&scrolled(&body)));
            load_address(ctx, wallet, &view);
        }
    }
    connect_retry(ctx, wallet, &view);
    toolbar.add_top_bar(&header);
    toasts.set_child(Some(&toolbar));
    let dialog = adw::Dialog::builder()
        .title("Receive")
        .content_width(420)
        .content_height(match wallet {
            WalletKind::Spending => 560,
            WalletKind::Saving => 620,
        })
        .child(&toasts)
        .build();
    dialog.present(Some(&ctx.nav));
}
