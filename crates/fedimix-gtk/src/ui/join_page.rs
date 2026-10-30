//! First-run page: join a federation from the suggestion list or an invite.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use fedimix_core::FedimixError;

use crate::present;
use crate::ui::app_state::AppContext;

type JoinedCallback = Box<dyn Fn()>;

struct Inner {
    ctx: Rc<AppContext>,
    form: gtk::Box,
    suggested: gtk::ListBox,
    entry: adw::EntryRow,
    error: gtk::Label,
    on_joined: RefCell<Option<JoinedCallback>>,
}

#[derive(Clone)]
pub struct JoinPage {
    root: adw::StatusPage,
    inner: Rc<Inner>,
}

impl JoinPage {
    pub fn new(ctx: &Rc<AppContext>) -> Self {
        let suggested = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .visible(false)
            .build();
        suggested.add_css_class("boxed-list");

        let entry = adw::EntryRow::builder()
            .title("Paste an invite code")
            .show_apply_button(false)
            .build();
        let join = gtk::Button::with_label("Join");
        join.set_valign(gtk::Align::Center);
        join.add_css_class("suggested-action");
        entry.add_suffix(&join);
        let invite_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        invite_list.add_css_class("boxed-list");
        invite_list.append(&entry);

        let error = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .build();
        error.add_css_class("error");

        let form = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        form.append(&suggested);
        form.append(&invite_list);
        form.append(&error);
        let clamp = adw::Clamp::builder()
            .maximum_size(420)
            .tightening_threshold(360)
            .child(&form)
            .build();

        let root = adw::StatusPage::builder()
            .icon_name("network-workgroup-symbolic")
            .title("Join a federation")
            .description("Fedimix keeps your spending money as ecash in a federation.")
            .child(&clamp)
            .vexpand(true)
            .build();

        let inner = Rc::new(Inner {
            ctx: ctx.clone(),
            form,
            suggested,
            entry,
            error,
            on_joined: RefCell::new(None),
        });

        let weak = Rc::downgrade(&inner);
        let submit = {
            let weak = weak.clone();
            move || {
                if let Some(inner) = weak.upgrade() {
                    let invite = inner.entry.text().trim().to_string();
                    inner.join(invite);
                }
            }
        };
        join.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        inner.entry.connect_entry_activated(move |_| submit());
        inner.entry.connect_changed({
            let weak = weak.clone();
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.clear_error();
                }
            }
        });

        inner.load_suggestions();
        // The widget owns the state, like the tabs; `Inner` holds no
        // reference to `root`, so this is not a cycle.
        root.connect_destroy({
            let inner = inner.clone();
            move |_| {
                let _keep_alive = &inner;
            }
        });
        Self { root, inner }
    }

    pub fn widget(&self) -> &adw::StatusPage {
        &self.root
    }

    /// Run `f` after a federation was joined successfully.
    pub fn connect_joined(&self, f: impl Fn() + 'static) {
        *self.inner.on_joined.borrow_mut() = Some(Box::new(f));
    }
}

impl Inner {
    fn clear_error(&self) {
        self.error.set_visible(false);
        self.entry.remove_css_class("error");
    }

    fn load_suggestions(self: &Rc<Self>) {
        let core = self.ctx.bridge.core();
        let weak = Rc::downgrade(self);
        self.ctx.run(
            async move { core.suggested_federations().await },
            move |list| {
                let Some(inner) = weak.upgrade() else { return };
                for fed in list {
                    let row = adw::ActionRow::builder()
                        .title(&fed.name)
                        .subtitle("Suggested")
                        .build();
                    let button = gtk::Button::with_label("Join");
                    button.set_valign(gtk::Align::Center);
                    button.connect_clicked({
                        let weak = Rc::downgrade(&inner);
                        let invite = fed.invite.clone();
                        move |_| {
                            if let Some(inner) = weak.upgrade() {
                                inner.join(invite.clone());
                            }
                        }
                    });
                    row.add_suffix(&button);
                    inner.suggested.append(&row);
                }
                inner
                    .suggested
                    .set_visible(inner.suggested.first_child().is_some());
            },
        );
    }

    fn join(self: &Rc<Self>, invite: String) {
        if invite.is_empty() {
            self.show_error("Paste an invite code first");
            return;
        }
        self.clear_error();
        // Insensitive while the join runs, so it cannot be started twice.
        self.form.set_sensitive(false);
        let core = self.ctx.bridge.core();
        let weak = Rc::downgrade(self);
        let ctx = self.ctx.clone();
        self.ctx.bridge.call(
            async move { core.join_federation(invite).await },
            move |res| {
                if let Some(inner) = weak.upgrade() {
                    inner.form.set_sensitive(true);
                    match &res {
                        Ok(_) => {
                            if let Some(f) = inner.on_joined.borrow().as_ref() {
                                f();
                            }
                        }
                        Err(e @ FedimixError::InvalidInvite(_)) => {
                            inner.show_error(&present::user_message(e));
                        }
                        Err(e) => ctx.toast(&present::user_message(e)),
                    }
                } else if let Err(e) = &res {
                    ctx.toast(&present::user_message(e));
                }
                if res.is_ok() {
                    ctx.refresh();
                }
            },
        );
    }

    fn show_error(&self, text: &str) {
        self.error.set_label(text);
        self.error.set_visible(true);
        self.entry.add_css_class("error");
    }
}
