//! Notes page: the Spending balance broken down by denomination.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use fedimix_core::NoteBucket;

use crate::format;
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::coins_page::{nav_page, page_body};

struct Inner {
    list: gtk::ListBox,
    last: RefCell<Option<Vec<NoteBucket>>>,
}

pub fn push(ctx: &Rc<AppContext>) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .build();
    list.add_css_class("boxed-list");
    let page = nav_page("Balance", &page_body(&list));

    let inner = Rc::new(Inner {
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
    page.connect_destroy(move |_| {
        let _keep_alive = &inner;
    });
    ctx.nav.push(&page);
}

impl Inner {
    fn update(&self, s: &Snapshots) {
        if self.last.borrow().as_ref() == Some(&s.spending.notes) {
            return;
        }
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if s.spending.notes.is_empty() {
            let empty = gtk::Label::builder()
                .label("No notes")
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
        for bucket in &s.spending.notes {
            let row = adw::ActionRow::builder()
                .title(format!(
                    "{} \u{d7} {}",
                    bucket.count,
                    format::btc(bucket.denomination)
                ))
                .build();
            self.list.append(&row);
        }
        *self.last.borrow_mut() = Some(s.spending.notes.clone());
    }
}
