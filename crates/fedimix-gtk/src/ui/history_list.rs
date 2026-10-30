//! Boxed list of history rows shared by both wallets.

use adw::prelude::*;

use crate::present::HistoryRowView;

#[derive(Clone)]
pub struct HistoryList {
    list: gtk::ListBox,
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
    gtk::ListBoxRow::builder()
        .child(&content)
        .activatable(false)
        .build()
}

impl HistoryList {
    pub fn new() -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        let this = Self { list };
        this.set_items(&[]);
        this
    }

    pub fn widget(&self) -> &gtk::ListBox {
        &self.list
    }

    /// Replace all rows; shows "No activity yet" when `items` is empty.
    pub fn set_items(&self, items: &[HistoryRowView]) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if items.is_empty() {
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
        for item in items {
            self.list.append(&build_row(item));
        }
    }
}
