//! Shared application context: snapshot cache, fan-out and error toasts.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;

use fedimix_core::{QueueState, SavingState, SpendingState};

use crate::bridge::CoreBridge;

#[derive(Debug, Clone)]
pub struct Snapshots {
    pub saving: SavingState,
    pub spending: SpendingState,
    pub queue: QueueState,
}

type Subscriber = Rc<dyn Fn(&Snapshots)>;

#[derive(Default)]
struct Shared {
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
            shared: Rc::default(),
        }
    }

    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    /// Register `f` to run on every new snapshot set, and once immediately if
    /// a snapshot is already available.
    ///
    /// There is no unsubscribe: views are pushed and popped, so callers must
    /// capture only weak references (`glib::clone!(#[weak] ...)` or
    /// `Weak<T>`) and let the closure no-op once the view is gone.
    pub fn on_snapshots(&self, f: impl Fn(&Snapshots) + 'static) {
        let f: Subscriber = Rc::new(f);
        self.shared.subscribers.borrow_mut().push(f.clone());
        if let Some(latest) = self.latest() {
            f(&latest);
        }
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
                Err(e) => ctx.toast(&format!("Could not load wallet state: {e}")),
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
        for f in subscribers {
            f(&snapshots);
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
            Err(e) => ctx.toast(&e.to_string()),
        });
    }
}
