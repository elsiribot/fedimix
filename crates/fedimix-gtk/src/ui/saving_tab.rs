//! The Saving tab: cold-storage balance, actions and history.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use fedimix_core::{SavingState, WalletKind};

use crate::present;
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::history_list::HistoryList;
use crate::ui::{coins_page, setup_saving, widgets};

struct Inner {
    pages: gtk::Stack,
    empty: adw::StatusPage,
    setup: gtk::Button,
    setup_note: gtk::Label,
    device_label: gtk::Label,
    balance: widgets::AmountButton,
    in_flight: gtk::Button,
    history: HistoryList,
    /// Last state rendered, to skip identical rebuilds.
    last: RefCell<Option<SavingState>>,
}

#[derive(Clone)]
pub struct SavingTab {
    root: gtk::ScrolledWindow,
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
        device_row.append(&gtk::Image::from_icon_name("media-removable-symbolic"));
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
        hero.append(&widgets::send_receive_row(&ctx, WalletKind::Saving));

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
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .build();

        let inner = Rc::new(Inner {
            pages,
            empty,
            setup,
            setup_note,
            device_label,
            balance,
            in_flight,
            history,
            last: RefCell::new(None),
        });
        let weak: Weak<Inner> = Rc::downgrade(&inner);
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

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.root
    }
}

impl Inner {
    fn update(&self, s: &Snapshots) {
        if self.last.borrow().as_ref() == Some(&s.saving) {
            return;
        }
        let saving = &s.saving;
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
                self.device_label.set_label(&present::device_line(device));
                self.balance.set_amount(&crate::format::btc(saving.balance));
                match present::in_flight_line(saving) {
                    Some(line) => {
                        self.in_flight.set_label(&line);
                        self.in_flight.set_visible(true);
                    }
                    None => self.in_flight.set_visible(false),
                }
                self.history.set_snapshot(&saving.history, true);
            }
        }
        *self.last.borrow_mut() = Some(saving.clone());
    }
}
