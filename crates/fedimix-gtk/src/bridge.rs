//! Bridge between the GTK main loop and the tokio runtime that runs the core.

use std::future::Future;
use std::sync::Arc;

use fedimix_core::{Backend, Event, Fedimix, FedimixError};

#[derive(Clone)]
pub struct CoreBridge {
    core: Fedimix,
    handle: tokio::runtime::Handle,
    events: async_channel::Receiver<Event>,
}

impl CoreBridge {
    /// Start the multi-thread runtime, parked on its own thread named
    /// "fedimix-core". The real backend needs worker threads: fedimint uses
    /// `block_in_place`, which panics on a current-thread runtime.
    pub fn start(backend: Arc<dyn Backend>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        let handle = rt.handle().clone();
        std::thread::Builder::new()
            .name("fedimix-core".to_string())
            .spawn(move || rt.block_on(std::future::pending::<()>()))
            .expect("failed to spawn core thread");

        let core = Fedimix::new(backend);
        let (tx, events) = async_channel::unbounded();
        let stream = core.subscribe();
        handle.spawn(async move {
            while let Some(event) = stream.next().await {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        Self {
            core,
            handle,
            events,
        }
    }

    pub fn core(&self) -> Fedimix {
        self.core.clone()
    }

    /// Run `fut` on the core runtime and deliver its result to `on_done` on
    /// the GTK main loop. Must be called from the main thread.
    ///
    /// `on_done` always runs: a future that panics (or is cancelled) yields
    /// `Err(FedimixError::Internal(..))`, so state guarded by a call, such as
    /// a refresh gate or a locked dialog, cannot be left held.
    pub fn call<T, Fut, Cb>(&self, fut: Fut, on_done: Cb)
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, FedimixError>> + Send + 'static,
        Cb: FnOnce(Result<T, FedimixError>) + 'static,
    {
        let task = self.handle.spawn(fut);
        gtk::glib::spawn_future_local(async move {
            let res = match task.await {
                Ok(res) => res,
                Err(_) => Err(FedimixError::Internal(
                    "Something went wrong inside Fedimix.".to_string(),
                )),
            };
            on_done(res);
        });
    }

    /// The core's events. Clones of this receiver share one queue, so there
    /// must be a single consumer: the window's event loop.
    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fedimix_core::{DemoBackend, DemoScenario};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    /// Run `body` with a private glib context as this thread's default (the
    /// shared default context can be held by only one test thread), then
    /// iterate it until `done`.
    fn on_private_context(body: impl FnOnce(), done: impl Fn() -> bool) {
        let ctx = gtk::glib::MainContext::new();
        ctx.with_thread_default(|| {
            body();
            let start = std::time::Instant::now();
            while !done() {
                assert!(start.elapsed().as_secs() < 5, "callback never fired");
                ctx.iteration(false);
            }
        })
        .unwrap();
    }

    #[test]
    fn call_delivers_on_main_context() {
        let bridge = CoreBridge::start(DemoBackend::new(DemoScenario::Funded));
        let got = Rc::new(Cell::new(None));
        let main_thread = std::thread::current().id();
        on_private_context(
            || {
                let g = got.clone();
                bridge.call(async { Ok::<_, FedimixError>(7u32) }, move |v| {
                    assert_eq!(std::thread::current().id(), main_thread);
                    g.set(Some(v.unwrap()));
                });
            },
            || got.get().is_some(),
        );
        assert_eq!(got.get(), Some(7));
    }

    #[test]
    fn panicking_future_reports_internal() {
        let bridge = CoreBridge::start(DemoBackend::new(DemoScenario::Funded));
        let got = Rc::new(RefCell::new(None));
        on_private_context(
            || {
                let g = got.clone();
                bridge.call(
                    async {
                        if true {
                            panic!("boom");
                        }
                        Ok::<u32, FedimixError>(1)
                    },
                    move |v| *g.borrow_mut() = Some(v),
                );
            },
            || got.borrow().is_some(),
        );
        assert_eq!(
            got.borrow().clone(),
            Some(Err(FedimixError::Internal(
                "Something went wrong inside Fedimix.".to_string()
            )))
        );
    }
}
