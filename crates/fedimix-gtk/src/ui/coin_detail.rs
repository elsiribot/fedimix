//! Detail page for one coin, with a Freeze switch.

use std::cell::Cell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use fedimix_core::{Coin, PrivacyTag};

use crate::format;
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::coins_page::{nav_page, page_body};

struct Inner {
    freeze: adw::SwitchRow,
    outpoint: String,
    /// Set while the switch is changed from a snapshot, not by the user.
    syncing: Cell<bool>,
}

fn row(title: &str, value: &str, selectable: bool) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .subtitle_selectable(selectable)
        .build();
    row.add_css_class("property");
    row
}

fn anonymity_at_exit(tag: &PrivacyTag) -> String {
    match tag {
        PrivacyTag::Anonymity { estimate } => format!("~{estimate}"),
        PrivacyTag::EarlyExit { estimate } => format!("~{estimate} (early exit)"),
        PrivacyTag::Unmixed => "None (unmixed)".to_string(),
        PrivacyTag::SmallChange => "None (small change)".to_string(),
    }
}

pub fn push(ctx: &Rc<AppContext>, coin: Coin) {
    let group = adw::PreferencesGroup::new();
    group.add(&row("Amount", &format::btc_exact(coin.amount), false));
    group.add(&row("Transaction", &coin.outpoint, true));
    group.add(&row("Address", &coin.address, true));
    group.add(&row(
        "Source federation",
        coin.source_federation.as_deref().unwrap_or("None"),
        false,
    ));
    group.add(&row(
        "Anonymity at exit",
        &anonymity_at_exit(&coin.tag),
        false,
    ));
    let freeze = adw::SwitchRow::builder()
        .title("Freeze")
        .subtitle("Frozen coins are never selected for spending")
        .active(coin.frozen)
        .build();
    group.add(&freeze);

    let page = nav_page("Coin", &page_body(&group));
    let inner = Rc::new(Inner {
        freeze: freeze.clone(),
        outpoint: coin.outpoint.clone(),
        syncing: Cell::new(false),
    });

    freeze.connect_active_notify({
        let ctx = ctx.clone();
        let weak = Rc::downgrade(&inner);
        let outpoint = coin.outpoint.clone();
        move |sw| {
            let Some(inner) = weak.upgrade() else { return };
            if inner.syncing.get() {
                return;
            }
            let frozen = sw.is_active();
            let core = ctx.bridge.core();
            let outpoint = outpoint.clone();
            let weak = Rc::downgrade(&inner);
            let ctx2 = ctx.clone();
            ctx.bridge.call(
                async move { core.set_coin_frozen(outpoint, frozen).await },
                move |res| {
                    if let Err(e) = res {
                        ctx2.toast(&e.to_string());
                        if let Some(inner) = weak.upgrade() {
                            inner.set_frozen(!frozen);
                        }
                    }
                },
            );
        }
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
    fn set_frozen(&self, frozen: bool) {
        if self.freeze.is_active() != frozen {
            self.syncing.set(true);
            self.freeze.set_active(frozen);
            self.syncing.set(false);
        }
    }

    fn update(&self, s: &Snapshots) {
        if let Some(coin) = s.saving.coins.iter().find(|c| c.outpoint == self.outpoint) {
            self.set_frozen(coin.frozen);
        }
    }
}
