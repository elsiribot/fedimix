//! Shared application context: snapshot cache, fan-out and error toasts.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;

use fedimix_core::{OperationId, OperationState, QueueState, SavingState, SpendingState};

use crate::bridge::CoreBridge;
use crate::operations::OperationTable;
use crate::present::user_message;

#[derive(Debug, Clone)]
pub struct Snapshots {
    pub saving: SavingState,
    pub spending: SpendingState,
    pub queue: QueueState,
}

type Subscriber = Rc<dyn Fn(&Snapshots) -> bool>;
type OperationWatcher = Rc<dyn Fn(&OperationState)>;

struct Shared {
    operations: RefCell<OperationTable<OperationWatcher>>,
    latest: RefCell<Option<Snapshots>>,
    subscribers: RefCell<Vec<Subscriber>>,
    /// Number of refreshes started so far.
    started: Cell<u64>,
    /// Sequence number of the newest refresh whose result was applied.
    applied: Cell<u64>,
}

#[derive(Clone)]
pub struct AppContext {
    pub bridge: CoreBridge,
    pub toasts: adw::ToastOverlay,
    pub nav: adw::NavigationView,
    pub stack: adw::ViewStack,
    shared: Rc<Shared>,
}

impl AppContext {
    pub fn new(
        bridge: CoreBridge,
        toasts: adw::ToastOverlay,
        nav: adw::NavigationView,
        stack: adw::ViewStack,
    ) -> Self {
        Self {
            bridge,
            toasts,
            nav,
            stack,
            shared: Rc::new(Shared {
                operations: RefCell::new(OperationTable::new()),
                latest: RefCell::default(),
                subscribers: RefCell::default(),
                started: Cell::default(),
                applied: Cell::default(),
            }),
        }
    }

    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    /// Show `message` now, ahead of any toast on screen. For outcomes, which
    /// must not wait behind "started".
    pub fn toast_now(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_priority(adw::ToastPriority::High);
        self.toasts.add_toast(toast);
    }

    /// Call `f` with each state of operation `id`, up to the final one. This
    /// outlives the dialog that started the operation, so `f` is where its
    /// outcome gets reported.
    pub fn watch_operation(&self, id: OperationId, f: impl Fn(&OperationState) + 'static) {
        let f: OperationWatcher = Rc::new(f);
        let replay = self.shared.operations.borrow_mut().watch(id, Rc::clone(&f));
        if let Some(state) = replay {
            f(&state);
        }
    }

    /// Feed an `Event::Operation` to its watcher.
    pub fn operation_event(&self, id: &str, state: &OperationState) {
        let watcher = self.shared.operations.borrow_mut().record(id, state);
        if let Some(f) = watcher {
            f(state);
        }
    }

    /// Register `f` to run on every new snapshot set, and once immediately if
    /// a snapshot is already available.
    ///
    /// `f` returns whether it wants to stay subscribed; returning `false`
    /// removes it. Views are pushed and popped, so callers must capture only
    /// weak references (`glib::clone!(#[weak] ...)` or `Weak<T>`) and return
    /// `false` once the view is gone.
    pub fn on_snapshots(&self, f: impl Fn(&Snapshots) -> bool + 'static) {
        let f: Subscriber = Rc::new(f);
        if let Some(latest) = self.latest()
            && !f(&latest)
        {
            return;
        }
        self.shared.subscribers.borrow_mut().push(f);
    }

    pub fn latest(&self) -> Option<Snapshots> {
        self.shared.latest.borrow().clone()
    }

    /// Refetch all three snapshots. A result that arrives after a newer
    /// refresh was already applied is discarded.
    pub fn refresh(&self) {
        let seq = self.shared.started.get() + 1;
        self.shared.started.set(seq);
        let core = self.bridge.core();
        let ctx = self.clone();
        self.bridge.call(
            async move {
                let (saving, spending, queue) = tokio::join!(
                    core.saving_state(),
                    core.spending_state(),
                    core.queue_state()
                );
                Ok::<_, fedimix_core::FedimixError>(Snapshots {
                    saving: saving?,
                    spending: spending?,
                    queue: queue?,
                })
            },
            move |res| match res {
                Ok(snapshots) => ctx.apply(seq, snapshots),
                Err(e) => ctx.toast(&format!(
                    "Could not load wallet state. {}",
                    user_message(&e)
                )),
            },
        );
    }

    fn apply(&self, seq: u64, snapshots: Snapshots) {
        if seq <= self.shared.applied.get() {
            return;
        }
        self.shared.applied.set(seq);
        *self.shared.latest.borrow_mut() = Some(snapshots.clone());
        // Clone the list so subscribers may register more subscribers.
        let subscribers: Vec<Subscriber> = self.shared.subscribers.borrow().clone();
        let mut dead: Vec<Subscriber> = Vec::new();
        for f in subscribers {
            if !f(&snapshots) {
                dead.push(f);
            }
        }
        if !dead.is_empty() {
            self.shared
                .subscribers
                .borrow_mut()
                .retain(|f| !dead.iter().any(|d| Rc::ptr_eq(f, d)));
        }
    }

    /// Run a core call; errors become a toast.
    pub fn run<T: Send + 'static>(
        &self,
        fut: impl Future<Output = fedimix_core::Result<T>> + Send + 'static,
        on_ok: impl FnOnce(T) + 'static,
    ) {
        let ctx = self.clone();
        self.bridge.call(fut, move |res| match res {
            Ok(v) => on_ok(v),
            Err(e) => ctx.toast(&user_message(&e)),
        });
    }
}
