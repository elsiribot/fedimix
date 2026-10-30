//! Refill dialog: move coins from Saving into Spending.

use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::RefillPreview;

use crate::format;
use crate::present;
use crate::ui::app_state::AppContext;

/// Prepare a refill and show the confirmation dialog. A failed preparation
/// is a toast and no dialog. `expanded` starts with the coin list revealed.
pub fn open(ctx: &Rc<AppContext>, expanded: bool) {
    let core = ctx.bridge.core();
    let ctx = ctx.clone();
    ctx.bridge
        .call(async move { core.prepare_refill().await }, {
            let ctx = ctx.clone();
            move |res| match res {
                Ok(preview) => show(&ctx, preview, expanded),
                Err(e) => ctx.toast(&present::user_message(&e)),
            }
        });
}

fn show(ctx: &Rc<AppContext>, preview: RefillPreview, expanded: bool) {
    let dialog = adw::AlertDialog::new(Some("Refill Spending"), None);

    let amount = gtk::Label::new(Some(&format::btc(preview.amount)));
    amount.add_css_class("title-2");
    let chevron = gtk::Label::new(Some(if expanded { "\u{2304}" } else { "\u{203a}" }));
    chevron.add_css_class("title-2");
    chevron.add_css_class("dim-label");
    let toggle_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    toggle_content.append(&amount);
    toggle_content.append(&chevron);
    let toggle = gtk::Button::builder()
        .child(&toggle_content)
        .halign(gtk::Align::Center)
        .build();
    toggle.add_css_class("flat");

    let source = gtk::Label::new(Some(&format!(
        "from Saving \u{b7} fee ~{}",
        format::sats(preview.fee)
    )));
    source.add_css_class("dim-label");

    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(6)
        .build();
    let heading = gtk::Label::builder()
        .label("Coins used (smallest first):")
        .xalign(0.0)
        .build();
    heading.add_css_class("dim-label");
    details.append(&heading);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    for coin in &preview.coins {
        let row = adw::ActionRow::builder()
            .title(format::btc_plain(coin.amount))
            .build();
        let tag = present::privacy_tag(&coin.tag);
        let pill = gtk::Label::new(Some(&tag.text));
        pill.add_css_class(tag.css_class);
        pill.set_valign(gtk::Align::Center);
        row.add_suffix(&pill);
        list.append(&row);
    }
    details.append(&list);
    let note_text = present::refill_overshoot_note(preview.overshoot);
    if !note_text.is_empty() {
        let note = gtk::Label::builder()
            .label(note_text)
            .wrap(true)
            .xalign(0.0)
            .build();
        note.add_css_class("dim-label");
        note.add_css_class("caption");
        details.append(&note);
    }
    let revealer = gtk::Revealer::builder()
        .reveal_child(expanded)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .child(&details)
        .build();
    toggle.connect_clicked({
        let revealer = revealer.downgrade();
        let chevron = chevron.downgrade();
        move |_| {
            let (Some(revealer), Some(chevron)) = (revealer.upgrade(), chevron.upgrade()) else {
                return;
            };
            let open = !revealer.reveals_child();
            revealer.set_reveal_child(open);
            chevron.set_label(if open { "\u{2304}" } else { "\u{203a}" });
        }
    });

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    body.append(&toggle);
    body.append(&source);
    body.append(&revealer);
    dialog.set_extra_child(Some(&body));

    let (text, enabled) = present::confirm_button(
        fedimix_core::WalletKind::Saving,
        ctx.latest().and_then(|s| s.saving.device).as_ref(),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("confirm", &text)]);
    dialog.set_response_enabled("confirm", enabled);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    // Follow the device while the dialog is open.
    ctx.on_snapshots({
        let dialog = dialog.downgrade();
        move |s| {
            let Some(dialog) = dialog.upgrade() else {
                return false;
            };
            let (text, enabled) =
                present::confirm_button(fedimix_core::WalletKind::Saving, s.saving.device.as_ref());
            dialog.set_response_label("confirm", &text);
            dialog.set_response_enabled("confirm", enabled);
            true
        }
    });

    dialog.connect_response(None, {
        let ctx = ctx.clone();
        let id = preview.id.clone();
        move |_, response| {
            if response != "confirm" {
                return;
            }
            let core = ctx.bridge.core();
            let id = id.clone();
            let done = ctx.clone();
            ctx.bridge
                .call(async move { core.confirm_refill(id).await }, move |res| {
                    match res {
                        Ok(()) => done.toast("Refill confirmed"),
                        Err(e) => done.toast(&present::user_message(&e)),
                    }
                    done.refresh();
                });
        }
    });
    dialog.present(Some(&ctx.nav));
}
