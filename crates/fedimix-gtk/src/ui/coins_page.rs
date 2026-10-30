//! Coins page: the Saving wallet's UTXOs, each opening a detail page.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use fedimix_core::Coin;

use crate::format;
use crate::present;
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::{coin_detail, widgets};

struct Inner {
    ctx: Rc<AppContext>,
    list: gtk::ListBox,
    last: RefCell<Option<Vec<Coin>>>,
}

/// Wrap `child` in the standard scrolled, width-clamped page body.
pub(crate) fn page_body(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .child(child)
        .build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build()
}

/// Wrap `body` in a toolbar view with a header and return the nav page.
pub(crate) fn nav_page(title: &str, body: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(body));
    adw::NavigationPage::new(&toolbar, title)
}

pub fn push(ctx: &Rc<AppContext>) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .build();
    list.add_css_class("boxed-list");
    let page = nav_page("Coins", &page_body(&list));

    let inner = Rc::new(Inner {
        ctx: ctx.clone(),
        list,
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
    // The page owns its state; see `SavingTab::new`.
    page.connect_destroy(move |_| {
        let _keep_alive = &inner;
    });
    ctx.nav.push(&page);
}

impl Inner {
    fn update(&self, s: &Snapshots) {
        if self.last.borrow().as_ref() == Some(&s.saving.coins) {
            return;
        }
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if s.saving.coins.is_empty() {
            let empty = gtk::Label::builder()
                .label("No coins")
                .margin_top(16)
                .margin_bottom(16)
                .build();
            empty.add_css_class("dim-label");
            self.list.append(
                &gtk::ListBoxRow::builder()
                    .child(&empty)
                    .activatable(false)
                    .build(),
            );
        }
        let now = widgets::now();
        for coin in &s.saving.coins {
            self.list.append(&self.build_row(coin, now));
        }
        *self.last.borrow_mut() = Some(s.saving.coins.clone());
    }

    fn build_row(&self, coin: &Coin, now: u64) -> adw::ActionRow {
        let mut date = format::date(coin.received_at, now);
        if coin.frozen {
            date.push_str(" \u{b7} frozen");
        }
        let row = adw::ActionRow::builder()
            .title(format::btc(coin.amount))
            .subtitle(date)
            .activatable(true)
            .build();
        let tag = present::privacy_tag(&coin.tag);
        let pill = gtk::Label::new(Some(&tag.text));
        pill.add_css_class(tag.css_class);
        pill.set_valign(gtk::Align::Center);
        row.add_suffix(&pill);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.connect_activated({
            let ctx = self.ctx.clone();
            let coin = coin.clone();
            move |_| coin_detail::push(&ctx, coin.clone())
        });
        row
    }
}
