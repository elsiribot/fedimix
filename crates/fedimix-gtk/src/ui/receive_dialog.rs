//! Receive dialog: Lightning invoice and on-chain address with QR.

use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::{Amount, ReceiveInfo, ReceiveMethod, ReceiveRequest, WalletKind, parse_btc};
use gtk::glib;

use crate::present::user_message;
use crate::ui::app_state::AppContext;
use crate::ui::qr;

/// QR, selectable payload text and a Copy button.
#[derive(Clone)]
struct PayloadView {
    root: gtk::Box,
    picture: gtk::Picture,
    text: gtk::Label,
    copy: gtk::Button,
    status: gtk::Label,
    content: gtk::Box,
}

impl PayloadView {
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
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();
        content.append(&picture);
        content.append(&text);
        content.append(&copy);
        let status = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .visible(false)
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        root.append(&status);
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
            status,
            content,
        }
    }

    fn show(&self, info: &ReceiveInfo) {
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
        self.copy.set_sensitive(true);
        self.content.set_visible(true);
    }

    fn show_status(&self, text: &str) {
        self.status.set_label(text);
        self.status.set_visible(true);
    }

    fn clear(&self) {
        self.status.set_visible(false);
        self.content.set_visible(false);
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
                Err(e) =>
                    view.show_status(&format!("Could not get an address. {}", user_message(&e))),
            }
        ),
    );
}

fn lightning_page(ctx: &Rc<AppContext>, toasts: &adw::ToastOverlay) -> gtk::Box {
    let amount = adw::EntryRow::builder().title("Amount (BTC)").build();
    amount.set_input_purpose(gtk::InputPurpose::Number);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    list.append(&amount);
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

    amount.connect_changed(glib::clone!(
        #[weak]
        create,
        #[strong]
        view,
        move |row| {
            // An edited amount makes the shown invoice stale.
            view.clear();
            create.set_sensitive(matches!(
                parse_btc(&row.text()),
                Ok(a) if a != Amount::ZERO
            ));
        }
    ));
    create.connect_clicked(glib::clone!(
        #[strong]
        ctx,
        #[weak]
        amount,
        #[strong]
        view,
        move |button| {
            let Ok(sats) = parse_btc(&amount.text()) else {
                return;
            };
            view.clear();
            button.set_sensitive(false);
            let core = ctx.bridge.core();
            let request = ReceiveRequest {
                wallet: WalletKind::Spending,
                method: ReceiveMethod::Lightning { amount: sats },
            };
            ctx.bridge.call(
                async move { core.receive(request).await },
                glib::clone!(
                    #[weak]
                    button,
                    #[strong]
                    view,
                    move |res| {
                        button.set_sensitive(true);
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
        }
    }
    load_address(ctx, wallet, &view);
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
