//! Settings page. Changes apply as they are made: spin rows and entries feed
//! one debounced write through `set_settings`.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use fedimix_core::{ChainBackend, FederationInfo, Settings};
use gtk::glib;

use crate::format;
use crate::present::{self, SettingsForm};
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::join_page::JoinPage;

/// Quiet period after the last edit before settings are written.
const APPLY_DELAY: Duration = Duration::from_millis(400);

struct Inner {
    ctx: Rc<AppContext>,
    federations: adw::PreferencesGroup,
    fed_rows: RefCell<Vec<adw::ActionRow>>,
    /// Federations and active id as last rendered.
    fed_shown: RefCell<Option<(Vec<FederationInfo>, Option<String>)>>,
    hw_name: adw::ActionRow,
    hw_status: adw::ActionRow,
    target: adw::EntryRow,
    anonymity: adw::SpinRow,
    min_hours: adw::SpinRow,
    grace: adw::SpinRow,
    fee_guard: adw::SpinRow,
    budget: adw::SpinRow,
    backend: adw::ComboRow,
    url: adw::EntryRow,
    proxy: adw::EntryRow,
    /// Last settings written (or loaded); fields the form lacks come from it.
    current: RefCell<Settings>,
    timer: RefCell<Option<glib::SourceId>>,
}

fn spin(title: &str, min: f64, max: f64, value: f64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, 1.0);
    row.set_title(title);
    row.set_value(value);
    row
}

fn entry(title: &str, text: &str) -> adw::EntryRow {
    let row = adw::EntryRow::builder().title(title).build();
    row.set_text(text);
    row
}

pub fn push(ctx: &Rc<AppContext>) {
    let core = ctx.bridge.core();
    ctx.run(
        async move {
            let (settings, federations) = tokio::join!(core.settings(), core.federations());
            Ok((settings?, federations?))
        },
        {
            let ctx = ctx.clone();
            move |(settings, federations)| show(&ctx, settings, federations)
        },
    );
}

fn show(ctx: &Rc<AppContext>, settings: Settings, federations: Vec<FederationInfo>) {
    let (backend_index, url) = match &settings.chain_backend {
        ChainBackend::Esplora { url } => (0, url.as_str()),
        ChainBackend::Electrum { url } => (1, url.as_str()),
    };
    let backend = adw::ComboRow::builder()
        .title("Server type")
        .model(&gtk::StringList::new(&["Esplora", "Electrum"]))
        .selected(backend_index)
        .build();

    let inner = Rc::new(Inner {
        ctx: ctx.clone(),
        federations: adw::PreferencesGroup::builder()
            .title("Federations")
            .build(),
        fed_rows: RefCell::default(),
        fed_shown: RefCell::default(),
        hw_name: adw::ActionRow::builder().title("Device").build(),
        hw_status: adw::ActionRow::builder().title("Status").build(),
        target: entry(
            "Target (BTC)",
            format::btc_exact(settings.spending_target)
                .trim_end_matches(" BTC")
                .trim_end_matches('0')
                .trim_end_matches('.'),
        ),
        anonymity: spin(
            "Anonymity target",
            1.0,
            1000.0,
            settings.anonymity_target.into(),
        ),
        min_hours: spin(
            "Minimum hours in pool",
            0.0,
            720.0,
            (settings.min_time_in_pool_secs / 3600) as f64,
        ),
        grace: spin(
            "Grace minutes",
            0.0,
            120.0,
            (settings.online_grace_secs / 60) as f64,
        ),
        fee_guard: spin(
            "Fee guard (%)",
            0.0,
            100.0,
            settings.fee_guard_percent.into(),
        ),
        budget: spin(
            "Budget (sat per month)",
            0.0,
            10_000_000.0,
            settings.cover_budget_sat_per_month as f64,
        ),
        backend,
        url: entry("Server URL", url),
        proxy: entry(
            "SOCKS5 proxy (optional)",
            settings.socks5_proxy.as_deref().unwrap_or(""),
        ),
        current: RefCell::new(settings),
        timer: RefCell::default(),
    });
    inner
        .target
        .set_tooltip_text(Some("In BTC, for example 0.05"));

    let page = adw::PreferencesPage::new();
    page.add(&inner.federations);

    let spending = adw::PreferencesGroup::builder().title("Spending").build();
    spending.add(&inner.target);
    page.add(&spending);

    let exit = adw::PreferencesGroup::builder().title("Exit").build();
    for row in [
        &inner.anonymity,
        &inner.min_hours,
        &inner.grace,
        &inner.fee_guard,
    ] {
        exit.add(row);
    }
    page.add(&exit);

    let cover = adw::PreferencesGroup::builder()
        .title("Cover traffic")
        .build();
    cover.add(&inner.budget);
    page.add(&cover);

    let hardware = adw::PreferencesGroup::builder()
        .title("Hardware wallet")
        .build();
    hardware.add(&inner.hw_name);
    hardware.add(&inner.hw_status);
    page.add(&hardware);

    let network = adw::PreferencesGroup::builder().title("Network").build();
    network.add(&inner.backend);
    network.add(&inner.url);
    network.add(&inner.proxy);
    page.add(&network);

    // Programmatic values are set above; handlers connect only now, so the
    // initial sync cannot trigger a write.
    let weak = Rc::downgrade(&inner);
    for row in [
        &inner.anonymity,
        &inner.min_hours,
        &inner.grace,
        &inner.fee_guard,
        &inner.budget,
    ] {
        row.connect_notify_local(Some("value"), {
            let weak = weak.clone();
            move |_, _| schedule(&weak)
        });
    }
    for row in [&inner.target, &inner.url, &inner.proxy] {
        row.connect_changed({
            let weak = weak.clone();
            move |_| schedule(&weak)
        });
    }
    inner.backend.connect_selected_notify({
        let weak = weak.clone();
        move |_| schedule(&weak)
    });

    inner.render_federations(&federations, ctx.latest().as_ref());

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    let nav_page = adw::NavigationPage::new(&toolbar, "Settings");

    ctx.on_snapshots({
        let weak = weak.clone();
        move |s| match weak.upgrade() {
            Some(inner) => {
                inner.update(s);
                true
            }
            None => false,
        }
    });
    // Popping within the debounce window must not lose the edit.
    nav_page.connect_hidden({
        let weak = weak.clone();
        move |_| flush(&weak)
    });
    // The page owns the state; snapshot subscribers hold it weakly.
    nav_page.connect_destroy(move |_| {
        let _keep_alive = &inner;
    });
    ctx.nav.push(&nav_page);
}

/// (Re)start the quiet period; the write happens when it expires.
fn schedule(weak: &Weak<Inner>) {
    let Some(inner) = weak.upgrade() else { return };
    if let Some(id) = inner.timer.borrow_mut().take() {
        id.remove();
    }
    let weak = weak.clone();
    let id = glib::timeout_add_local_once(APPLY_DELAY, move || {
        let Some(inner) = weak.upgrade() else { return };
        // The source is spent; forget it so it is never removed twice.
        inner.timer.borrow_mut().take();
        inner.apply();
    });
    *inner.timer.borrow_mut() = Some(id);
}

/// Write a pending debounced edit now.
fn flush(weak: &Weak<Inner>) {
    let Some(inner) = weak.upgrade() else { return };
    if let Some(id) = inner.timer.borrow_mut().take() {
        id.remove();
        inner.apply();
    }
}

impl Inner {
    fn apply(self: &Rc<Self>) {
        let target = self.target.text();
        let url = self.url.text();
        let proxy = self.proxy.text();
        let form = SettingsForm {
            target_btc: &target,
            anonymity_target: self.anonymity.value(),
            min_hours: self.min_hours.value(),
            grace_minutes: self.grace.value(),
            fee_guard_percent: self.fee_guard.value(),
            cover_budget_sat: self.budget.value(),
            electrum: self.backend.selected() == 1,
            url: &url,
            proxy: &proxy,
        };
        let result = present::settings_from_form(&form);
        let target_bad = format::parse_btc(&target).is_err();
        let url_bad = url.trim().is_empty();
        set_error(&self.target, target_bad);
        set_error(&self.url, url_bad);
        match result {
            Ok(settings) => {
                if *self.current.borrow() == settings {
                    return;
                }
                let core = self.ctx.bridge.core();
                let weak = Rc::downgrade(self);
                let saved = settings.clone();
                // `current` only advances once the write succeeded, so a
                // failed write is retried by the next edit.
                self.ctx.run(
                    async move { core.set_settings(settings).await },
                    move |()| {
                        if let Some(inner) = weak.upgrade() {
                            *inner.current.borrow_mut() = saved;
                        }
                    },
                );
            }
            Err(msg) => {
                self.target.set_tooltip_text(Some(if target_bad {
                    &msg
                } else {
                    "In BTC, for example 0.05"
                }));
            }
        }
    }

    fn update(self: &Rc<Self>, s: &Snapshots) {
        match &s.saving.device {
            Some(device) => {
                self.hw_name.set_subtitle(&device.name);
                self.hw_status.set_subtitle(if device.connected {
                    "Connected"
                } else {
                    "Not connected"
                });
            }
            None => {
                self.hw_name.set_subtitle("None");
                self.hw_status.set_subtitle("Not set up");
            }
        }
        // Refetch on every snapshot: a joined or changed federation may not
        // be the active one. `render_federations` skips an unchanged list.
        let core = self.ctx.bridge.core();
        let weak = Rc::downgrade(self);
        let snapshots = s.clone();
        self.ctx
            .run(async move { core.federations().await }, move |feds| {
                if let Some(inner) = weak.upgrade() {
                    inner.render_federations(&feds, Some(&snapshots));
                }
            });
    }

    fn render_federations(self: &Rc<Self>, feds: &[FederationInfo], s: Option<&Snapshots>) {
        let active = s
            .and_then(|s| s.spending.federation.as_ref())
            .map(|f| f.id.clone());
        let shown = (feds.to_vec(), active);
        if self.fed_shown.borrow().as_ref() == Some(&shown) {
            return;
        }
        let active = shown.1.as_deref();
        for row in self.fed_rows.borrow_mut().drain(..) {
            self.federations.remove(&row);
        }
        let mut group: Option<gtk::CheckButton> = None;
        let mut rows = Vec::new();
        for fed in feds {
            let radio = gtk::CheckButton::new();
            match &group {
                Some(first) => radio.set_group(Some(first)),
                None => group = Some(radio.clone()),
            }
            radio.set_active(Some(fed.id.as_str()) == active);
            let row = adw::ActionRow::builder()
                .title(&fed.name)
                .subtitle(if fed.online { "Online" } else { "Offline" })
                .activatable_widget(&radio)
                .build();
            row.add_prefix(&radio);
            // Connected after the initial state is set, so only user changes fire.
            radio.connect_toggled({
                let ctx = self.ctx.clone();
                let id = fed.id.clone();
                move |radio| {
                    if radio.is_active() {
                        let core = ctx.bridge.core();
                        let id = id.clone();
                        ctx.run(async move { core.set_active_federation(id).await }, |()| {});
                    }
                }
            });
            self.federations.add(&row);
            rows.push(row);
        }
        let add = adw::ActionRow::builder()
            .title("Add federation")
            .activatable(true)
            .build();
        add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
        add.connect_activated({
            let ctx = self.ctx.clone();
            move |_| push_join(&ctx)
        });
        self.federations.add(&add);
        rows.push(add);
        *self.fed_rows.borrow_mut() = rows;
        *self.fed_shown.borrow_mut() = Some(shown);
    }
}

fn set_error(row: &adw::EntryRow, bad: bool) {
    if bad {
        row.add_css_class("error");
    } else {
        row.remove_css_class("error");
    }
}

pub(crate) fn push_join(ctx: &Rc<AppContext>) {
    let join = JoinPage::new(ctx);
    join.connect_joined({
        let nav = ctx.nav.clone();
        move || {
            nav.pop();
        }
    });
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(join.widget()));
    ctx.nav
        .push(&adw::NavigationPage::new(&toolbar, "Add federation"));
}
