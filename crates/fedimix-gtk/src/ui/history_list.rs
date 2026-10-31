//! Boxed list of history rows shared by both wallets, with "Show more".

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use fedimix_core::{HistoryItem, WalletKind};

use crate::present::{self, HistoryRowView};
use crate::ui::app_state::AppContext;
use crate::ui::widgets;

const SHOW_MORE: &str = "Show more";
const LOADING: &str = "Loading\u{2026}";

struct Inner {
    ctx: Rc<AppContext>,
    wallet: WalletKind,
    list: gtk::ListBox,
    more_label: gtk::Label,
    more_row: gtk::ListBoxRow,
    /// The newest rows, from the last snapshot.
    snapshot: RefCell<Vec<HistoryItem>>,
    /// Pages loaded below the snapshot, oldest last.
    pages: RefCell<Vec<HistoryItem>>,
    /// Older rows may exist behind the last row.
    more: Cell<bool>,
    loading: Cell<bool>,
    /// Bumped when the pages are dropped, so a late reply is ignored.
    generation: Cell<u64>,
    show_cover: Cell<bool>,
}

#[derive(Clone)]
pub struct HistoryList {
    inner: Rc<Inner>,
}

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build()
}

fn build_row(item: &HistoryRowView) -> gtk::ListBoxRow {
    let line = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    line.append(&gtk::Label::new(Some(item.icon)));
    let title = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .hexpand(true)
        .build();
    title.append(&label(item.title));
    if let Some(tag) = &item.tag {
        let tag_label = label(&tag.text);
        tag_label.add_css_class(tag.css_class);
        tag_label.set_valign(gtk::Align::Center);
        title.append(&tag_label);
    }
    line.append(&title);
    let amount = gtk::Label::new(Some(&item.amount));
    amount.add_css_class("numeric");
    line.append(&amount);
    let trailing = gtk::Label::builder()
        .label(&item.trailing)
        .xalign(1.0)
        .width_chars(7)
        .build();
    trailing.add_css_class("dim-label");
    line.append(&trailing);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&line);
    if let Some(nested) = &item.nested {
        // Drawn inside the parent row: indented, no card of its own.
        let nested_label = label(nested);
        nested_label.set_margin_start(28);
        nested_label.add_css_class("dim-label");
        nested_label.add_css_class("caption");
        content.append(&nested_label);
    }
    if let Some(detail) = &item.detail {
        // Selectable, as a recipient may ask for the txid.
        let detail_label = gtk::Label::builder()
            .label(detail)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::Char)
            .selectable(true)
            .margin_start(28)
            .build();
        detail_label.add_css_class("dim-label");
        detail_label.add_css_class("caption");
        content.append(&detail_label);
    }
    gtk::ListBoxRow::builder()
        .child(&content)
        .activatable(false)
        .build()
}

impl HistoryList {
    pub fn new(ctx: &Rc<AppContext>, wallet: WalletKind) -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        let more_label = gtk::Label::builder()
            .label(SHOW_MORE)
            .margin_top(12)
            .margin_bottom(12)
            .build();
        more_label.add_css_class("accent");
        let more_row = gtk::ListBoxRow::builder()
            .child(&more_label)
            .activatable(true)
            .build();
        let inner = Rc::new(Inner {
            ctx: ctx.clone(),
            wallet,
            list,
            more_label,
            more_row,
            snapshot: RefCell::default(),
            pages: RefCell::default(),
            more: Cell::new(false),
            loading: Cell::new(false),
            generation: Cell::new(0),
            show_cover: Cell::new(true),
        });
        let weak: Weak<Inner> = Rc::downgrade(&inner);
        inner.list.connect_row_activated(move |_, row| {
            if let Some(inner) = weak.upgrade()
                && *row == inner.more_row
            {
                inner.load_more();
            }
        });
        inner.render();
        Self { inner }
    }

    pub fn widget(&self) -> &gtk::ListBox {
        &self.inner.list
    }

    /// Show `snapshot` (the newest rows), plus the pages already loaded
    /// below it. Cover rows only appear with `show_cover`.
    pub fn set_snapshot(&self, snapshot: &[HistoryItem], show_cover: bool) {
        let inner = &self.inner;
        inner.show_cover.set(show_cover);
        let lined_up = present::extra_pages_line_up(&inner.snapshot.borrow(), snapshot);
        if !lined_up {
            inner.pages.borrow_mut().clear();
            inner.generation.set(inner.generation.get() + 1);
            inner.loading.set(false);
            inner
                .more
                .set(present::history_may_have_more(snapshot.len()));
        }
        *inner.snapshot.borrow_mut() = snapshot.to_vec();
        inner.render();
    }
}

impl Inner {
    fn render(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let now = widgets::now();
        let shown = present::history_to_show(
            &self.snapshot.borrow(),
            &self.pages.borrow(),
            self.show_cover.get(),
        );
        if shown.is_empty() && !self.more.get() {
            let empty = gtk::Label::builder()
                .label("No activity yet")
                .margin_top(16)
                .margin_bottom(16)
                .build();
            empty.add_css_class("dim-label");
            self.list.append(
                &gtk::ListBoxRow::builder()
                    .child(&empty)
                    .activatable(false)
                    .selectable(false)
                    .build(),
            );
            return;
        }
        for item in &shown {
            self.list
                .append(&build_row(&present::history_row(item, now)));
        }
        if self.more.get() {
            self.more_label.set_label(if self.loading.get() {
                LOADING
            } else {
                SHOW_MORE
            });
            self.more_row.set_activatable(!self.loading.get());
            self.list.append(&self.more_row);
        }
    }

    /// Fetch the page below the last row held.
    fn load_more(self: &Rc<Self>) {
        if self.loading.replace(true) {
            return;
        }
        let cursor = self
            .pages
            .borrow()
            .last()
            .map(|i| i.id.clone())
            .or_else(|| self.snapshot.borrow().last().map(|i| i.id.clone()));
        self.render();
        let generation = self.generation.get();
        let core = self.ctx.bridge.core();
        let wallet = self.wallet;
        let weak = Rc::downgrade(self);
        let ctx = self.ctx.clone();
        self.ctx.bridge.call(
            async move { core.history(wallet, cursor, present::HISTORY_PAGE).await },
            move |res| {
                let Some(inner) = weak.upgrade() else { return };
                if inner.generation.get() != generation {
                    return;
                }
                inner.loading.set(false);
                match res {
                    Ok(page) => {
                        inner.pages.borrow_mut().extend(page.items);
                        inner.more.set(page.more);
                    }
                    Err(e) => ctx.toast(&present::user_message(&e)),
                }
                inner.render();
            },
        );
    }
}
