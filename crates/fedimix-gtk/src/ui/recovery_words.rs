//! Recovery words page: the seed, shown on request, with a warning.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::ui::app_state::AppContext;
use crate::ui::coins_page::{nav_page, page_body};

const WARNING: &str = "Write these words down and keep them offline. Anyone who has them can \
take your bitcoin, and without them a lost device cannot be recovered. Never share or \
photograph them.";

/// Fetch the words and show them on a pushed page.
pub fn push(ctx: &Rc<AppContext>) {
    let core = ctx.bridge.core();
    ctx.run(async move { core.recovery_words().await }, {
        let ctx = ctx.clone();
        move |words| show(&ctx, words)
    });
}

fn show(ctx: &Rc<AppContext>, words: String) {
    let warning = gtk::Label::builder()
        .label(WARNING)
        .wrap(true)
        .xalign(0.0)
        .build();
    warning.add_css_class("warning");

    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(3)
        .row_spacing(6)
        .column_spacing(6)
        .build();
    for (n, word) in words.split_whitespace().enumerate() {
        let chip = gtk::Label::builder()
            .label(format!("{}. {word}", n + 1))
            .xalign(0.0)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(10)
            .margin_end(10)
            .build();
        chip.add_css_class("monospace");
        chip.add_css_class("card");
        grid.insert(&chip, -1);
    }

    let copy = gtk::Button::with_label("Copy");
    copy.add_css_class("pill");
    copy.set_halign(gtk::Align::Center);
    // Held in the closure only until the page goes away.
    let secret = Rc::new(std::cell::RefCell::new(words));
    copy.connect_clicked(glib::clone!(
        #[strong]
        ctx,
        #[strong]
        secret,
        move |button| {
            button.clipboard().set_text(&secret.borrow());
            ctx.toast("Copied");
        }
    ));

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .build();
    body.append(&warning);
    body.append(&grid);
    body.append(&copy);

    let page = nav_page("Recovery words", &page_body(&body));
    // Do not keep the secret in widgets or memory once the page is left.
    page.connect_hidden(move |_| {
        secret.borrow_mut().clear();
        while let Some(child) = grid.first_child() {
            grid.remove(&child);
        }
    });
    ctx.nav.push(&page);
}
