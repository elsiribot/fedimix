use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;

use crate::bridge::CoreBridge;
use crate::cli::StartPage;
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::saving_tab::SavingTab;
use crate::ui::spending_tab::SpendingTab;
use crate::ui::{coin_detail, coins_page, moving_page, notes_page};

const MIN_WIDTH: i32 = 360;
const MIN_HEIGHT: i32 = 480;

/// Main application window.
pub struct FedimixWindow {
    window: adw::ApplicationWindow,
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

impl FedimixWindow {
    pub fn new(app: &adw::Application, bridge: CoreBridge, start_page: Option<StartPage>) -> Self {
        load_css();

        let stack = adw::ViewStack::new();
        let saving_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        stack.add_titled_with_icon(
            &saving_slot,
            Some("saving"),
            "Saving",
            "security-high-symbolic",
        );
        let spending_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        stack.add_titled_with_icon(
            &spending_slot,
            Some("spending"),
            "Spending",
            "emblem-system-symbolic",
        );

        let header_switcher = adw::ViewSwitcher::builder()
            .policy(adw::ViewSwitcherPolicy::Wide)
            .stack(&stack)
            .build();
        let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).build();

        let menu = gio::Menu::new();
        menu.append(Some("Settings"), Some("app.settings"));
        menu.append(Some("About"), Some("app.about"));
        menu.append(Some("Quit"), Some("app.quit"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&menu)
            .build();

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&header_switcher));
        header.pack_end(&menu_button);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&stack));
        toolbar.add_bottom_bar(&switcher_bar);

        let root = adw::NavigationPage::new(&toolbar, "Fedimix");
        let nav = adw::NavigationView::new();
        nav.add(&root);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Fedimix")
            .default_width(480)
            .default_height(640)
            .width_request(MIN_WIDTH)
            .height_request(MIN_HEIGHT)
            .content(&toasts)
            .build();

        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            500.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
        breakpoint.add_setter(&header_switcher, "visible", Some(&false.to_value()));
        window.add_breakpoint(breakpoint);

        let ctx = Rc::new(AppContext::new(bridge, toasts, nav, stack));
        let saving_tab = SavingTab::new(ctx.clone());
        saving_tab.widget().set_vexpand(true);
        saving_slot.append(saving_tab.widget());
        let spending_tab = SpendingTab::new(ctx.clone());
        spending_tab.widget().set_vexpand(true);
        spending_slot.append(spending_tab.widget());
        Self::install_actions(app, &ctx);
        Self::wire_state(&ctx, start_page);
        ctx.refresh();

        Self { window }
    }

    fn install_actions(app: &adw::Application, ctx: &AppContext) {
        let quit = gio::ActionEntry::builder("quit")
            .activate(|app: &adw::Application, _, _| app.quit())
            .build();
        // Settings and About pages arrive with later tasks.
        let settings = gio::ActionEntry::builder("settings")
            .activate({
                let ctx = ctx.clone();
                move |_: &adw::Application, _, _| ctx.toast("Settings are not available yet")
            })
            .build();
        let about = gio::ActionEntry::builder("about")
            .activate({
                let ctx = ctx.clone();
                move |_: &adw::Application, _, _| ctx.toast("About is not available yet")
            })
            .build();
        app.add_action_entries([quit, settings, about]);
    }

    /// Pick the initial tab and `--demo-page` page from the first snapshot.
    /// Later tasks add their own pages here.
    fn apply_start_page(ctx: &Rc<AppContext>, page: Option<StartPage>, s: &Snapshots) {
        let default_tab = if s.saving.device.is_some() {
            "saving"
        } else {
            "spending"
        };
        let tab = match page {
            // Forced so the Saving empty state can be screenshotted.
            Some(StartPage::Saving) => "saving",
            Some(StartPage::Spending) => "spending",
            _ => default_tab,
        };
        ctx.stack.set_visible_child_name(tab);
        match page {
            Some(StartPage::Coins) => coins_page::push(ctx),
            Some(StartPage::CoinDetail) => {
                if let Some(coin) = s.saving.coins.first() {
                    coin_detail::push(ctx, coin.clone());
                }
            }
            Some(StartPage::Notes) => notes_page::push(ctx),
            Some(StartPage::Moving) => moving_page::push(ctx),
            _ => {}
        }
    }

    fn wire_state(ctx: &Rc<AppContext>, start_page: Option<StartPage>) {
        let picked = Rc::new(Cell::new(false));
        ctx.on_snapshots({
            let ctx = ctx.clone();
            move |s| {
                if !picked.replace(true) {
                    Self::apply_start_page(&ctx, start_page, s);
                }
                false
            }
        });

        // Any backend event invalidates the snapshots.
        let events = ctx.bridge.events();
        let ctx = ctx.clone();
        gtk::glib::spawn_future_local(async move {
            while events.recv().await.is_ok() {
                ctx.refresh();
            }
        });
    }

    pub fn present(&self) {
        self.window.present();
    }
}
