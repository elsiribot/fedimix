//! The Spending tab: ecash balance, refill/queue links and history.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use fedimix_core::{SpendingState, WalletKind};

use crate::present;
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::history_list::HistoryList;
use crate::ui::{moving_page, notes_page, refill_dialog, setup_saving, widgets};

struct Inner {
    offline: adw::Banner,
    nudge: adw::Banner,
    federation: gtk::Label,
    balance: widgets::AmountButton,
    bar: gtk::LevelBar,
    caption: gtk::Label,
    caption_link: gtk::Button,
    refill: gtk::Button,
    hint: gtk::Label,
    history: HistoryList,
    show_cover: gtk::CheckButton,
    /// Last state rendered, to skip identical rebuilds.
    last: RefCell<Option<SpendingState>>,
}

#[derive(Clone)]
pub struct SpendingTab {
    root: gtk::Box,
}

impl SpendingTab {
    pub fn new(ctx: Rc<AppContext>) -> Self {
        let offline = adw::Banner::builder().title("Offline").build();
        let nudge = adw::Banner::builder()
            .button_label("Set up \u{203a}")
            .build();
        nudge.connect_button_clicked({
            let ctx = ctx.clone();
            move |_| setup_saving::open(&ctx)
        });

        let federation = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        federation.add_css_class("dim-label");

        let balance = widgets::amount_button("hero-balance");
        balance.connect_clicked({
            let ctx = ctx.clone();
            move || notes_page::push(&ctx)
        });

        let bar = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .build();
        for offset in ["low", "high", "full"] {
            bar.remove_offset_value(Some(offset));
        }
        let bar_clamp = adw::Clamp::builder()
            .maximum_size(200)
            .tightening_threshold(200)
            .child(&bar)
            .build();

        let caption = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        caption.add_css_class("dim-label");
        let caption_link = gtk::Button::builder().halign(gtk::Align::Center).build();
        caption_link.add_css_class("flat");
        caption_link.add_css_class("link");
        caption_link.connect_clicked({
            let ctx = ctx.clone();
            move |_| moving_page::push(&ctx)
        });

        let refill = gtk::Button::with_label("Refill from Saving");
        refill.add_css_class("flat");
        refill.add_css_class("link");
        refill.set_halign(gtk::Align::Center);
        refill.connect_clicked({
            let ctx = ctx.clone();
            move |_| refill_dialog::open(&ctx, false)
        });

        let hint = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .visible(false)
            .build();
        hint.add_css_class("caption");
        hint.add_css_class("warning");

        let hero = widgets::hero_box();
        hero.append(&federation);
        hero.append(balance.widget());
        hero.append(&bar_clamp);
        hero.append(&caption);
        hero.append(&caption_link);
        hero.append(&refill);
        hero.append(&hint);
        hero.append(&widgets::send_receive_row(&ctx, WalletKind::Spending));

        let heading = gtk::Label::builder()
            .label("History")
            .xalign(0.0)
            .hexpand(true)
            .build();
        heading.add_css_class("heading");
        let show_cover = gtk::CheckButton::with_label("Show cover traffic");
        show_cover.add_css_class("dim-label");
        let heading_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        heading_row.append(&heading);
        heading_row.append(&show_cover);
        let history = HistoryList::new(&ctx, WalletKind::Spending);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(16)
            .margin_end(16)
            .build();
        content.append(&hero);
        content.append(&heading_row);
        content.append(history.widget());

        let clamp = adw::Clamp::builder()
            .maximum_size(600)
            .child(&content)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&offline);
        root.append(&nudge);
        root.append(&scroller);

        let inner = Rc::new(Inner {
            offline,
            nudge,
            federation,
            balance,
            bar,
            caption,
            caption_link,
            refill,
            hint,
            history,
            show_cover: show_cover.clone(),
            last: RefCell::new(None),
        });
        let weak: Weak<Inner> = Rc::downgrade(&inner);
        show_cover.connect_toggled({
            let weak = weak.clone();
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.render();
                }
            }
        });
        ctx.on_snapshots(move |s| match weak.upgrade() {
            Some(inner) => {
                inner.update(s);
                true
            }
            None => false,
        });
        // The widget owns the state; see `SavingTab::new`.
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
        let hint = present::spending_hint(&s.queue);
        self.hint.set_visible(hint.is_some());
        self.hint.set_label(hint.as_deref().unwrap_or(""));
        if self.last.borrow().as_ref() == Some(&s.spending) {
            return;
        }
        *self.last.borrow_mut() = Some(s.spending.clone());
        self.render();
    }

    fn render(&self) {
        let last = self.last.borrow();
        let Some(s) = last.as_ref() else { return };
        // Without a federation the window shows the Join page instead.
        let offline = matches!(&s.federation, Some(f) if !f.online);
        self.offline.set_revealed(offline);
        match present::saving_banner(s) {
            Some(text) => {
                self.nudge.set_title(&text);
                self.nudge.set_revealed(true);
            }
            None => self.nudge.set_revealed(false),
        }
        self.federation.set_label(
            s.federation
                .as_ref()
                .map_or("No federation", |f| f.name.as_str()),
        );
        self.balance.set_amount(&crate::format::btc(s.balance));

        let caption = present::spending_caption(s);
        self.bar.set_value(caption.fraction);
        self.caption.set_label(&caption.text);
        self.caption.set_visible(!caption.text.is_empty());
        self.caption_link
            .set_label(caption.queue_link.as_deref().unwrap_or(""));
        self.caption_link.set_visible(caption.queue_link.is_some());
        self.refill.set_visible(caption.shows_refill);

        self.history
            .set_snapshot(&s.history, self.show_cover.is_active());
    }
}
