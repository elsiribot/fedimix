//! Recovery words page: the seed, shown on request, with a warning.

use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::present;
use crate::ui::app_state::AppContext;
use crate::ui::coins_page::{nav_page, page_body};

const COPIED: &str = "Copied. Clear your clipboard when done.";
/// How long the words may sit on the clipboard.
const CLIPBOARD_LIFETIME: Duration = Duration::from_secs(60);

/// Put `words` on the clipboard and take them off after `CLIPBOARD_LIFETIME`.
/// Only a weak reference waits for that, so whatever replaces them frees the
/// words at once and stays where it is.
fn copy_words(ctx: &AppContext, clipboard: gdk::Clipboard, words: &str) {
    let content = gdk::ContentProvider::for_value(&words.to_value());
    if clipboard.set_content(Some(&content)).is_err() {
        ctx.toast("Could not copy.");
        return;
    }
    ctx.toast(COPIED);
    let content = content.downgrade();
    glib::timeout_add_local_once(CLIPBOARD_LIFETIME, move || {
        if let Some(content) = content.upgrade()
            && clipboard.content().as_ref() == Some(&content)
        {
            let _ = clipboard.set_content(gdk::ContentProvider::NONE);
        }
    });
}

/// Remember that the words were opened, which ends the prompt on Spending.
fn mark_shown(ctx: &Rc<AppContext>) {
    if ctx.latest().is_some_and(|s| s.recovery_words_shown) {
        return;
    }
    let core = ctx.bridge.core();
    ctx.run(
        async move {
            let settings = core.settings().await?;
            core.set_settings(fedimix_core::Settings {
                recovery_words_shown: true,
                ..settings
            })
            .await
        },
        |()| {},
    );
}

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
        .label(present::RECOVERY_WARNING)
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
            copy_words(&ctx, button.clipboard(), &secret.borrow());
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
    mark_shown(ctx);
}
