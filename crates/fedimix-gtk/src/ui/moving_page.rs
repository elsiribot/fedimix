//! Moving to Saving page: the queue of notes waiting to leave Spending.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use fedimix_core::{ExitPreview, OperationState, QueueState, Settings};
use gtk::gio;

use crate::format;
use crate::present;
use crate::ui::app_state::{AppContext, Snapshots};
use crate::ui::coins_page::{nav_page, page_body};

struct Inner {
    ctx: Rc<AppContext>,
    total: gtk::Label,
    summary: gtk::Label,
    list: gtk::ListBox,
    /// Exits that may have left: what Fedimix cannot tell about them.
    unknown: gtk::Label,
    empty: adw::StatusPage,
    /// Anonymity target; the spec default until settings arrive.
    target: Cell<u32>,
    last: RefCell<Option<QueueState>>,
}

pub fn push(ctx: &Rc<AppContext>) {
    let total = gtk::Label::new(None);
    total.add_css_class("title-1");
    let summary = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    summary.add_css_class("dim-label");
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .build();
    list.add_css_class("boxed-list");
    let unknown = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .build();
    unknown.add_css_class("dim-label");
    let empty = adw::StatusPage::builder()
        .title("Nothing to move")
        .icon_name("emblem-ok-symbolic")
        .visible(false)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    content.append(&total);
    content.append(&summary);
    content.append(&list);
    content.append(&unknown);
    content.append(&empty);
    let page = nav_page("Moving to Saving", &page_body(&content));

    let inner = Rc::new(Inner {
        ctx: ctx.clone(),
        total,
        summary,
        list,
        unknown,
        empty,
        target: Cell::new(Settings::default().anonymity_target),
        last: RefCell::new(None),
    });
    let weak: Weak<Inner> = Rc::downgrade(&inner);
    ctx.run(
        {
            let core = ctx.bridge.core();
            async move { core.settings().await }
        },
        {
            let weak = weak.clone();
            move |settings| {
                if let Some(inner) = weak.upgrade() {
                    inner.target.set(settings.anonymity_target);
                    inner.render();
                }
            }
        },
    );
    ctx.on_snapshots(move |s| match weak.upgrade() {
        Some(inner) => {
            inner.update(s);
            true
        }
        None => false,
    });
    // The page owns its state; see `SavingTab::new`.
    page.connect_destroy(move |_| {
        let _keep_alive = &inner;
    });
    ctx.nav.push(&page);
}

impl Inner {
    fn update(&self, s: &Snapshots) {
        if self.last.borrow().as_ref() == Some(&s.queue) {
            return;
        }
        *self.last.borrow_mut() = Some(s.queue.clone());
        self.render();
    }

    fn render(&self) {
        let last = self.last.borrow();
        let Some(q) = last.as_ref() else { return };
        self.total.set_label(&format::btc(q.total));
        self.summary.set_label(&if q.waiting_for_saving {
            "Waiting for Saving".to_string()
        } else {
            present::queue_summary(q)
        });
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for item in &q.items {
            self.list
                .append(&self.build_row(item, q.waiting_for_saving));
        }
        let note = present::unknown_exits_note(q);
        self.unknown.set_label(note.as_deref().unwrap_or(""));
        self.unknown.set_visible(note.is_some());
        let none = q.items.is_empty();
        self.empty.set_visible(none);
        self.list.set_visible(!none);
        self.total.set_visible(!none);
        self.summary.set_visible(!none);
    }

    fn build_row(&self, item: &fedimix_core::QueueItem, waiting: bool) -> adw::ActionRow {
        let view = present::queue_row(item, self.target.get());
        let row = adw::ActionRow::builder()
            .title(view.label)
            .subtitle(view.detail)
            .build();
        let bar = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .value(view.fraction)
            .valign(gtk::Align::Center)
            .width_request(56)
            .build();
        for offset in ["low", "high", "full"] {
            bar.remove_offset_value(Some(offset));
        }
        row.add_suffix(&bar);

        let action = gio::SimpleAction::new("exit", None);
        action.connect_activate({
            let ctx = self.ctx.clone();
            let id = item.id.clone();
            let estimate = item.anonymity_current;
            move |_, _| confirm_exit(&ctx, id.clone(), estimate)
        });
        let group = gio::SimpleActionGroup::new();
        group.add_action(&action);
        let menu = gio::Menu::new();
        menu.append(Some("Exit now (lower privacy)"), Some("item.exit"));
        let button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&menu)
            .valign(gtk::Align::Center)
            .sensitive(!waiting && present::exit_allowed(&item.status))
            .tooltip_text("Item actions")
            .build();
        button.add_css_class("flat");
        button.insert_action_group("item", Some(&group));
        row.add_suffix(&button);
        row
    }
}

/// Quote the exit, then ask. A refused quote is a toast and no dialog.
/// `estimate` is the item's anonymity estimate as its row shows it.
fn confirm_exit(ctx: &Rc<AppContext>, item_id: String, estimate: u32) {
    let core = ctx.bridge.core();
    let shown = ctx.clone();
    ctx.run(
        async move { core.prepare_exit(item_id).await },
        move |preview| {
            show_exit_dialog(&shown, preview, estimate);
        },
    );
}

fn show_exit_dialog(ctx: &Rc<AppContext>, preview: ExitPreview, estimate: u32) {
    let warning = present::exit_warning(&preview, estimate);
    let dialog = adw::AlertDialog::new(Some("Exit now?"), Some(&warning));
    dialog.add_responses(&[("cancel", "Cancel"), ("exit", "Exit now")]);
    dialog.set_response_appearance("exit", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, {
        let ctx = ctx.clone();
        let id = preview.id;
        move |_, response| {
            let core = ctx.bridge.core();
            let id = id.clone();
            if response == "exit" {
                exit(&ctx, id);
            } else {
                ctx.bridge
                    .call(async move { core.discard_preview(id).await }, |_| {});
            }
        }
    });
    // Rows are rebuilt on every snapshot, so parent on the stable nav view.
    dialog.present(Some(&ctx.nav));
}

fn exit(ctx: &Rc<AppContext>, preview_id: String) {
    let core = ctx.bridge.core();
    let ctx = ctx.clone();
    ctx.clone()
        .bridge
        .call(async move { core.exit_now(preview_id).await }, move |res| {
            let id = match res {
                Ok(id) => id,
                Err(e) => {
                    ctx.toast(&present::user_message(&e));
                    return;
                }
            };
            let done = ctx.clone();
            ctx.watch_operation(id, move |state| match state {
                OperationState::Done => done.toast_now("Moved to Saving"),
                OperationState::Failed { error } => {
                    done.toast_now(&present::user_message(error));
                }
                _ => {}
            });
        });
}
