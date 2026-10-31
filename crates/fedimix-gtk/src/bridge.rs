//! Bridge between the GTK main loop and the tokio runtime that runs the core.

use std::future::Future;
use std::sync::Arc;

use fedimix_core::{Backend, CoreConfig, Event, Fedimix, FedimixError, RealBackend};

/// Why a call ended without a result. `user_message` puts "Something went
/// wrong:" in front, so this must not say it too.
const PANIC_WHY: &str = "a background task stopped.";

/// Most a clean shutdown may take; a hung backend must not block quitting.
pub const SHUTDOWN_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// `fut`, or an error once `cap` has passed.
pub async fn capped<Fut>(fut: Fut, cap: std::time::Duration) -> Result<(), FedimixError>
where
    Fut: Future<Output = Result<(), FedimixError>>,
{
    tokio::time::timeout(cap, fut).await.unwrap_or_else(|_| {
        Err(FedimixError::Internal(
            "shutdown took too long.".to_string(),
        ))
    })
}

/// Start the multi-thread runtime, parked on its own thread named
/// "fedimix-core". The real backend needs worker threads: fedimint uses
/// `block_in_place`, which panics on a current-thread runtime.
pub fn start_runtime() -> tokio::runtime::Handle {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let handle = rt.handle().clone();
    std::thread::Builder::new()
        .name("fedimix-core".to_string())
        .spawn(move || rt.block_on(std::future::pending::<()>()))
        .expect("failed to spawn core thread");
    handle
}

/// Open the real backend on the core runtime and wait for it. It only reads
/// the data folder, so this is quick and needs no network.
pub fn open_real(
    handle: &tokio::runtime::Handle,
    config: CoreConfig,
) -> Result<Arc<RealBackend>, FedimixError> {
    let task = handle.spawn(RealBackend::open(config));
    handle
        .block_on(task)
        .unwrap_or_else(|_| Err(FedimixError::Internal(PANIC_WHY.to_string())))
}

#[derive(Clone)]
pub struct CoreBridge {
    core: Fedimix,
    handle: tokio::runtime::Handle,
    events: async_channel::Receiver<Event>,
}

impl CoreBridge {
    /// Run `backend` on a new runtime.
    #[cfg(test)]
    pub fn start(backend: Arc<dyn Backend>) -> Self {
        Self::on(start_runtime(), backend)
    }

    /// Run `backend` on the runtime of `handle`.
    pub fn on(handle: tokio::runtime::Handle, backend: Arc<dyn Backend>) -> Self {
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
                Err(_) => Err(FedimixError::Internal(PANIC_WHY.to_string())),
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
        let err = got.borrow().clone().unwrap().unwrap_err();
        assert_eq!(err, FedimixError::Internal(PANIC_WHY.to_string()));
        // The user text already says "Something went wrong"; the reason must
        // not say it again.
        let text = crate::present::user_message(&err);
        assert_eq!(text, "Something went wrong: a background task stopped.");
        assert_eq!(text.matches("Something went wrong").count(), 1);
    }

    #[tokio::test]
    async fn shutdown_gives_up_on_a_hung_backend() {
        let started = std::time::Instant::now();
        let res = capped(
            std::future::pending::<Result<(), FedimixError>>(),
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(matches!(res, Err(FedimixError::Internal(_))));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let ok = capped(async { Ok::<_, FedimixError>(()) }, SHUTDOWN_CAP).await;
        assert_eq!(ok, Ok(()));
    }

    /// `RealBackend::shutdown` takes up to 25 s, and GTK must not cut it off.
    #[test]
    fn the_cap_outlasts_the_backends_own_limits() {
        assert_eq!(SHUTDOWN_CAP, std::time::Duration::from_secs(30));
    }
}
