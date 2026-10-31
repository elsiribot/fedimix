//! Settings page. Exit, cover and target changes apply as they are made,
//! through one debounced write. Network fields apply on request instead: a
//! half-typed server or proxy must never take effect.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use fedimix_core::{ChainBackend, FederationInfo, Settings, parse_btc};
use gtk::glib;

use crate::format;
use crate::present::{self, SettingsForm};
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::join_page::JoinPage;
use crate::ui::recovery_words;

/// Quiet period after the last edit before settings are written.
const APPLY_DELAY: Duration = Duration::from_millis(400);

struct Inner {
    ctx: Rc<AppContext>,
    federations: adw::PreferencesGroup,
    fed_rows: RefCell<Vec<adw::ActionRow>>,
    /// Federations and active id as last rendered.
    fed_shown: RefCell<Option<(Vec<FederationInfo>, Option<String>)>>,
    exit: adw::PreferencesGroup,
    cover: adw::PreferencesGroup,
    hardware: adw::PreferencesGroup,
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
    /// Network values in force. Debounced writes use these, not the fields.
    network: RefCell<Network>,
    /// Says what is wrong with the form, and offers to revert it.
    banner: adw::Banner,
    page: glib::WeakRef<adw::NavigationPage>,
    timer: RefCell<Option<glib::SourceId>>,
}

#[derive(Clone)]
struct Network {
    backend: ChainBackend,
    proxy: Option<String>,
}

impl Network {
    fn of(settings: &Settings) -> Self {
        Self {
            backend: settings.chain_backend.clone(),
            proxy: settings.socks5_proxy.clone(),
        }
    }

    fn is_electrum(&self) -> bool {
        matches!(self.backend, ChainBackend::Electrum { .. })
    }

    fn url(&self) -> &str {
        match &self.backend {
            ChainBackend::Esplora { url } | ChainBackend::Electrum { url } => url,
        }
    }
}

/// What an Apply writes.
enum NetworkEdit<'a> {
    /// Server type and URL, which only make sense together.
    Server {
        electrum: bool,
        url: &'a str,
    },
    Proxy(&'a str),
    /// Neither: the values in force, for writes of other settings.
    None,
}

/// Network values for the settings to write.
struct NetworkFields<'a> {
    electrum: bool,
    url: &'a str,
    proxy: &'a str,
}

impl Network {
    /// The edited value from `edit`, every other from the values in force.
    /// Half-typed or untouched fields never get through, and applying one
    /// row never clears (or changes) the other: that would drop a user's
    /// proxy without their say.
    fn with_edit<'a>(&'a self, edit: NetworkEdit<'a>) -> NetworkFields<'a> {
        let forced = NetworkFields {
            electrum: self.is_electrum(),
            url: self.url(),
            proxy: self.proxy.as_deref().unwrap_or(""),
        };
        match edit {
            NetworkEdit::Server { electrum, url } => NetworkFields {
                electrum,
                url,
                ..forced
            },
            NetworkEdit::Proxy(proxy) => NetworkFields { proxy, ..forced },
            NetworkEdit::None => forced,
        }
    }
}

fn target_text(settings: &Settings) -> String {
    format::btc_exact(settings.spending_target)
        .trim_end_matches(" BTC")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
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

fn network_entry(title: &str, text: &str) -> adw::EntryRow {
    let row = entry(title, text);
    row.set_show_apply_button(true);
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
    let network = Network::of(&settings);
    let backend = adw::ComboRow::builder()
        .title("Server type")
        .model(&gtk::StringList::new(&["Esplora", "Electrum"]))
        .selected(u32::from(network.is_electrum()))
        .build();
    let toolbar = adw::ToolbarView::new();
    let nav_page = adw::NavigationPage::new(&toolbar, "Settings");

    let inner = Rc::new(Inner {
        ctx: ctx.clone(),
        federations: adw::PreferencesGroup::builder()
            .title("Federations")
            .build(),
        fed_rows: RefCell::default(),
        fed_shown: RefCell::default(),
        // Hidden until a snapshot says the backend has them.
        exit: adw::PreferencesGroup::builder()
            .title("Exit")
            .visible(false)
            .build(),
        cover: adw::PreferencesGroup::builder()
            .title("Cover traffic")
            .visible(false)
            .build(),
        hardware: adw::PreferencesGroup::builder()
            .title("Hardware wallet")
            .visible(false)
            .build(),
        hw_name: adw::ActionRow::builder().title("Device").build(),
        hw_status: adw::ActionRow::builder().title("Status").build(),
        target: entry("Target (BTC)", &target_text(&settings)),
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
        url: network_entry("Server URL", network.url()),
        proxy: network_entry(
            "SOCKS5 proxy (optional)",
            network.proxy.as_deref().unwrap_or(""),
        ),
        current: RefCell::new(settings),
        network: RefCell::new(network),
        banner: adw::Banner::builder().button_label("Revert").build(),
        page: nav_page.downgrade(),
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

    for row in [
        &inner.anonymity,
        &inner.min_hours,
        &inner.grace,
        &inner.fee_guard,
    ] {
        inner.exit.add(row);
    }
    page.add(&inner.exit);

    inner.cover.add(&inner.budget);
    page.add(&inner.cover);

    inner.hardware.add(&inner.hw_name);
    inner.hardware.add(&inner.hw_status);
    page.add(&inner.hardware);

    let backup = adw::PreferencesGroup::builder().title("Backup").build();
    let words = adw::ActionRow::builder()
        .title("Recovery words")
        .activatable(true)
        .build();
    words.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    words.connect_activated({
        let ctx = ctx.clone();
        move |_| recovery_words::push(&ctx)
    });
    backup.add(&words);
    page.add(&backup);

    let network = adw::PreferencesGroup::builder()
        .title("Network")
        .description(present::PROXY_CAPTION)
        .build();
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
    inner.target.connect_changed({
        let weak = weak.clone();
        move |_| schedule(&weak)
    });
    for row in [&inner.url, &inner.proxy] {
        row.connect_changed({
            let weak = weak.clone();
            move |_| with(&weak, |inner| inner.validate())
        });
    }
    // Each Apply writes its own row; the other network value stays as it is.
    inner.url.connect_apply({
        let weak = weak.clone();
        move |_| with(&weak, Inner::apply_server)
    });
    inner.proxy.connect_apply({
        let weak = weak.clone();
        move |_| with(&weak, Inner::apply_proxy)
    });
    inner.backend.connect_selected_notify({
        let weak = weak.clone();
        move |_| with(&weak, Inner::apply_server)
    });
    inner.banner.connect_button_clicked({
        let weak = weak.clone();
        move |_| with(&weak, |inner| inner.revert())
    });

    inner.render_federations(&federations, ctx.latest().as_ref());

    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.add_top_bar(&inner.banner);
    toolbar.set_content(Some(&page));

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

fn with(weak: &Weak<Inner>, f: impl FnOnce(&Rc<Inner>)) {
    if let Some(inner) = weak.upgrade() {
        f(&inner);
    }
}

/// (Re)start the quiet period; the write happens when it expires.
fn schedule(weak: &Weak<Inner>) {
    let Some(inner) = weak.upgrade() else { return };
    inner.validate();
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
    /// The form's settings with the given network values.
    fn build(&self, electrum: bool, url: &str, proxy: &str) -> Result<Settings, String> {
        let target = self.target.text();
        present::settings_from_form(&SettingsForm {
            target_btc: &target,
            anonymity_target: self.anonymity.value(),
            min_hours: self.min_hours.value(),
            grace_minutes: self.grace.value(),
            fee_guard_percent: self.fee_guard.value(),
            cover_budget_sat: self.budget.value(),
            electrum,
            url,
            proxy,
        })
    }

    /// The settings the fields say, network included.
    fn field_settings(&self) -> Result<Settings, String> {
        self.build(
            self.backend.selected() == 1,
            &self.url.text(),
            &self.proxy.text(),
        )
    }

    /// Show what is wrong, if anything. An invalid form cannot be popped:
    /// leaving would lose it unsaid, so the banner offers Revert instead.
    fn validate(&self) {
        let electrum = self.backend.selected() == 1;
        let url = self.url.text();
        set_error(&self.target, parse_btc(&self.target.text()).is_err());
        set_error(
            &self.url,
            present::validate_server_url(electrum, &url).is_err(),
        );
        set_error(
            &self.proxy,
            present::validate_proxy(&self.proxy.text()).is_err(),
        );
        let problem = self.field_settings().err();
        self.banner.set_title(problem.as_deref().unwrap_or(""));
        self.banner.set_revealed(problem.is_some());
        if let Some(page) = self.page.upgrade() {
            page.set_can_pop(problem.is_none());
        }
    }

    /// Debounced write of everything but the network fields.
    fn apply(self: &Rc<Self>) {
        let network = self.network.borrow().clone();
        let f = network.with_edit(NetworkEdit::None);
        if let Ok(settings) = self.build(f.electrum, f.url, f.proxy) {
            self.write(settings);
        }
    }

    /// Server type and URL, which only make sense together. The proxy is
    /// left at the value in force, whatever its field holds.
    fn apply_server(self: &Rc<Self>) {
        let network = self.network.borrow().clone();
        let url = self.url.text();
        let f = network.with_edit(NetworkEdit::Server {
            electrum: self.backend.selected() == 1,
            url: &url,
        });
        let settings = self.build(f.electrum, f.url, f.proxy);
        self.apply_network(settings);
    }

    /// The proxy alone; the server stays at the value in force.
    fn apply_proxy(self: &Rc<Self>) {
        let network = self.network.borrow().clone();
        let proxy = self.proxy.text();
        let f = network.with_edit(NetworkEdit::Proxy(&proxy));
        let settings = self.build(f.electrum, f.url, f.proxy);
        self.apply_network(settings);
    }

    fn apply_network(self: &Rc<Self>, settings: Result<Settings, String>) {
        self.validate();
        if let Ok(settings) = settings {
            *self.network.borrow_mut() = Network::of(&settings);
            self.write(settings);
        }
    }

    fn write(self: &Rc<Self>, mut settings: Settings) {
        settings.recovery_words_shown = self.current.borrow().recovery_words_shown;
        if *self.current.borrow() == settings {
            return;
        }
        let core = self.ctx.bridge.core();
        let weak = Rc::downgrade(self);
        let saved = settings.clone();
        let ctx = self.ctx.clone();
        // `current` only advances once the write succeeded, so a failed
        // write is retried by the next edit.
        self.ctx.bridge.call(
            async move { core.set_settings(settings).await },
            move |res| {
                let inner = weak.upgrade();
                match res {
                    Ok(()) => {
                        if let Some(inner) = inner {
                            *inner.current.borrow_mut() = saved;
                        }
                    }
                    Err(e) => {
                        ctx.toast(&present::user_message(&e));
                        if let Some(inner) = inner {
                            let network = Network::of(&inner.current.borrow());
                            *inner.network.borrow_mut() = network;
                        }
                    }
                }
            },
        );
    }

    /// Put every field back to the values in force.
    fn revert(&self) {
        let network = self.network.borrow().clone();
        self.target.set_text(&target_text(&self.current.borrow()));
        self.url.set_text(network.url());
        self.proxy.set_text(network.proxy.as_deref().unwrap_or(""));
        self.backend.set_selected(u32::from(network.is_electrum()));
        self.validate();
    }

    fn update(self: &Rc<Self>, s: &Snapshots) {
        let features = s.spending.features;
        self.exit.set_visible(features.scheduler);
        self.cover.set_visible(features.cover);
        self.hardware.set_visible(features.saving);
        // Only Saving talks to a chain server.
        self.backend.set_visible(features.saving);
        self.url.set_visible(features.saving);
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

    /// Redraw the radios from the core, after a switch it refused.
    fn show_active_federation(self: &Rc<Self>) {
        *self.fed_shown.borrow_mut() = None;
        if let Some(s) = self.ctx.latest() {
            self.update(&s);
        }
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
                // The name comes from the federation, so it is not markup.
                .use_markup(false)
                .title(&fed.name)
                .subtitle(present::federation_subtitle(fed))
                .activatable_widget(&radio)
                .build();
            row.add_prefix(&radio);
            // Connected after the initial state is set, so only user changes fire.
            radio.connect_toggled({
                let ctx = self.ctx.clone();
                let id = fed.id.clone();
                let weak = Rc::downgrade(self);
                move |radio| {
                    if radio.is_active() {
                        let core = ctx.bridge.core();
                        let id = id.clone();
                        let done = ctx.clone();
                        let weak = weak.clone();
                        ctx.bridge.call(
                            async move { core.set_active_federation(id).await },
                            move |res| {
                                if let Err(e) = res {
                                    done.toast(&present::user_message(&e));
                                    with(&weak, Inner::show_active_federation);
                                }
                            },
                        );
                    }
                }
            });
            self.federations.add(&row);
            rows.push(row);
            if let Some(offer) = present::federation_invite(fed) {
                let invite = adw::ActionRow::builder()
                    .use_markup(false)
                    .title("Invite code")
                    .subtitle(&offer.shown)
                    .subtitle_selectable(true)
                    .build();
                let copy = gtk::Button::with_label("Copy");
                copy.set_valign(gtk::Align::Center);
                copy.connect_clicked({
                    let ctx = self.ctx.clone();
                    move |button| {
                        button.clipboard().set_text(&offer.copy);
                        ctx.toast("Invite code copied.");
                    }
                });
                invite.add_suffix(&copy);
                self.federations.add(&invite);
                rows.push(invite);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    const PROXY: &str = "127.0.0.1:9050";

    fn in_force() -> Network {
        Network {
            backend: ChainBackend::Electrum {
                url: "ssl://electrum.example:50002".into(),
            },
            proxy: Some(PROXY.into()),
        }
    }

    /// The settings a form with these network fields would write.
    fn written(fields: &NetworkFields) -> Settings {
        present::settings_from_form(&SettingsForm {
            target_btc: "0.05",
            anonymity_target: 50.0,
            min_hours: 24.0,
            grace_minutes: 10.0,
            fee_guard_percent: 2.0,
            cover_budget_sat: 20_000.0,
            electrum: fields.electrum,
            url: fields.url,
            proxy: fields.proxy,
        })
        .unwrap()
    }

    #[test]
    fn applying_the_server_never_clears_the_proxy() {
        let net = in_force();
        let fields = net.with_edit(NetworkEdit::Server {
            electrum: false,
            url: "https://esplora.example/api",
        });
        let s = written(&fields);
        assert_eq!(s.socks5_proxy.as_deref(), Some(PROXY));
        assert_eq!(
            s.chain_backend,
            ChainBackend::Esplora {
                url: "https://esplora.example/api".into()
            }
        );
    }

    #[test]
    fn applying_the_proxy_leaves_the_server_alone() {
        let net = in_force();
        let fields = net.with_edit(NetworkEdit::Proxy("10.0.0.1:1080"));
        let s = written(&fields);
        assert_eq!(s.socks5_proxy.as_deref(), Some("10.0.0.1:1080"));
        assert_eq!(
            s.chain_backend,
            ChainBackend::Electrum {
                url: "ssl://electrum.example:50002".into()
            }
        );
    }

    #[test]
    fn only_an_explicit_empty_proxy_edit_clears_it() {
        let net = in_force();
        let cleared = net.with_edit(NetworkEdit::Proxy(""));
        assert_eq!(written(&cleared).socks5_proxy, None);
        let untouched = net.with_edit(NetworkEdit::None);
        assert_eq!(written(&untouched).socks5_proxy.as_deref(), Some(PROXY));
    }
}
