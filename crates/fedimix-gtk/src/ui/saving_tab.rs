//! The Saving tab: cold-storage balance, actions and history.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use fedimix_core::{Features, SavingState, WalletKind};

use crate::present;
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::history_list::HistoryList;
use crate::ui::{coins_page, settings_page, setup_saving, widgets};

struct Inner {
    /// The chain server's problem or being offline, and what fixes it.
    banner: adw::Banner,
    fix: std::cell::Cell<Option<present::SavingFix>>,
    pages: gtk::Stack,
    empty: adw::StatusPage,
    setup: gtk::Button,
    setup_note: gtk::Label,
    device_icon: gtk::Image,
    device_label: gtk::Label,
    balance: widgets::AmountButton,
    in_flight: gtk::Button,
    send: gtk::Button,
    receive: gtk::Button,
    heading: gtk::Label,
    history: HistoryList,
    /// Last state rendered, to skip identical rebuilds.
    last: RefCell<Option<(SavingState, Features)>>,
}

#[derive(Clone)]
pub struct SavingTab {
    root: gtk::Box,
}

impl SavingTab {
    pub fn new(ctx: Rc<AppContext>) -> Self {
        let device_label = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let device_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::Center)
            .build();
        device_row.add_css_class("dim-label");
        let device_icon = gtk::Image::from_icon_name("media-removable-symbolic");
        device_row.append(&device_icon);
        device_row.append(&device_label);

        let balance = widgets::amount_button("hero-balance");
        balance.connect_clicked({
            let ctx = ctx.clone();
            move || coins_page::push(&ctx)
        });

        let in_flight = gtk::Button::builder().halign(gtk::Align::Center).build();
        in_flight.add_css_class("flat");
        in_flight.add_css_class("link");
        in_flight.connect_clicked({
            let ctx = ctx.clone();
            move |_| ctx.stack.set_visible_child_name("spending")
        });

        let hero = widgets::hero_box();
        hero.append(&device_row);
        hero.append(balance.widget());
        hero.append(&in_flight);
        let (actions, send, receive) = widgets::send_receive_buttons(&ctx, WalletKind::Saving);
        hero.append(&actions);

        let heading = gtk::Label::builder().label("History").xalign(0.0).build();
        heading.add_css_class("heading");
        let history = HistoryList::new(&ctx, WalletKind::Saving);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(16)
            .margin_end(16)
            .build();
        content.append(&hero);
        content.append(&heading);
        content.append(history.widget());

        let setup = gtk::Button::with_label("Set up");
        setup.add_css_class("pill");
        setup.add_css_class("suggested-action");
        setup.set_halign(gtk::Align::Center);
        setup.connect_clicked({
            let ctx = ctx.clone();
            move |_| setup_saving::open(&ctx)
        });
        let setup_note = gtk::Label::builder().visible(false).build();
        setup_note.add_css_class("dim-label");
        let setup_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        setup_box.append(&setup);
        setup_box.append(&setup_note);
        let empty = adw::StatusPage::builder()
            .icon_name("media-removable-symbolic")
            .description("Saving is your cold storage. Spending works without it.")
            .child(&setup_box)
            .vexpand(true)
            .build();

        let pages = gtk::Stack::new();
        pages.add_named(&content, Some("content"));
        pages.add_named(&empty, Some("empty"));

        let clamp = adw::Clamp::builder()
            .maximum_size(600)
            .child(&pages)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        let banner = adw::Banner::new("");
        // A chain server's words can reach it, so it is not markup.
        banner.set_use_markup(false);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&banner);
        root.append(&scroll);

        let inner = Rc::new(Inner {
            banner: banner.clone(),
            fix: std::cell::Cell::new(None),
            pages,
            empty,
            setup,
            setup_note,
            device_icon,
            device_label,
            balance,
            in_flight,
            send,
            receive,
            heading,
            history,
            last: RefCell::new(None),
        });
        let weak: Weak<Inner> = Rc::downgrade(&inner);
        banner.connect_button_clicked({
            let ctx = ctx.clone();
            let weak = weak.clone();
            move |_| match weak.upgrade().and_then(|inner| inner.fix.get()) {
                Some(present::SavingFix::SetUpAgain) => {
                    if let Some(device) = ctx.latest().and_then(|s| s.saving.device) {
                        setup_saving::open_again(&ctx, device);
                    }
                }
                Some(present::SavingFix::Settings) => settings_page::push(&ctx),
                None => {}
            }
        });
        ctx.on_snapshots(move |s| match weak.upgrade() {
            Some(inner) => {
                inner.update(s);
                true
            }
            None => false,
        });
        // The widget owns the state: snapshot subscribers hold it weakly, so
        // it lives exactly as long as the widget and callers need not keep
        // the `SavingTab` handle around. `Inner` never references `root`, so
        // this is not a cycle.
        root.connect_destroy(move |_| {
            let _keep_alive = &inner;
        });
        Self { root }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}

impl Inner {
    fn update(&self, s: &Snapshots) {
        let shown = (s.saving.clone(), s.spending.features);
        if self.last.borrow().as_ref() == Some(&shown) {
            return;
        }
        let saving = &s.saving;
        let banner = present::saving_banner_view(saving);
        self.fix.set(banner.as_ref().and_then(|b| b.button));
        self.banner
            .set_title(banner.as_ref().map_or("", |b| b.text.as_str()));
        self.banner.set_button_label(
            banner
                .as_ref()
                .and_then(|b| b.button)
                .map(present::SavingFix::label),
        );
        self.banner.set_revealed(banner.is_some());
        match &saving.device {
            None => {
                let empty = present::saving_empty_state(s.spending.features.saving);
                self.empty.set_title(empty.title);
                self.setup.set_label(empty.button);
                self.setup.set_sensitive(empty.button_enabled);
                self.setup_note.set_label(empty.caption.unwrap_or(""));
                self.setup_note.set_visible(empty.caption.is_some());
                self.pages.set_visible_child_name("empty");
            }
            Some(device) => {
                self.pages.set_visible_child_name("content");
                self.device_icon
                    .set_icon_name(Some(present::device_icon(device)));
                self.device_label.set_label(&present::device_line(device));
                // Until the wallet opens and has scanned, Saving's figures are
                // not known: the banner says why, and no number stands in.
                let known = present::saving_figures_known(saving);
                self.balance.set_amount(&if known {
                    crate::format::btc(saving.balance)
                } else {
                    "\u{2014}".to_string()
                });
                self.balance.widget().set_sensitive(known);
                self.send.set_sensitive(known);
                // Receive waits out a scan by itself; an unopened wallet has
                // no addresses to give.
                self.receive.set_sensitive(saving.wallet_open);
                self.heading.set_visible(known);
                self.history.widget().set_visible(known);
                match present::in_flight_line(saving, s.spending.features) {
                    Some(line) => {
                        self.in_flight.set_label(&line);
                        self.in_flight.set_visible(true);
                    }
                    None => self.in_flight.set_visible(false),
                }
                self.history.set_snapshot(&saving.history, true);
            }
        }
        *self.last.borrow_mut() = Some(shown);
    }
}
