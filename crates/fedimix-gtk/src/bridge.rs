//! Bridge between the GTK main loop and the tokio runtime that runs the core.

use std::future::Future;
use std::sync::Arc;

use fedimix_core::{Backend, Event, Fedimix};

#[derive(Clone)]
pub struct CoreBridge {
    core: Fedimix,
    handle: tokio::runtime::Handle,
    events: async_channel::Receiver<Event>,
}

impl CoreBridge {
    /// Start the runtime on its own thread named "fedimix-core".
    pub fn start(backend: Arc<dyn Backend>) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
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
    pub fn call<T, Fut, Cb>(&self, fut: Fut, on_done: Cb)
    where
        T: Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        Cb: FnOnce(T) + 'static,
    {
        let (tx, rx) = async_channel::bounded(1);
        self.handle.spawn(async move {
            let _ = tx.send(fut.await).await;
        });
        gtk::glib::spawn_future_local(async move {
            if let Ok(value) = rx.recv().await {
                on_done(value);
            }
        });
    }

    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fedimix_core::{DemoBackend, DemoScenario};
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn call_delivers_on_main_context() {
        let bridge = CoreBridge::start(DemoBackend::new(DemoScenario::Funded));
        let ctx = gtk::glib::MainContext::default();
        let _guard = ctx.acquire().unwrap();
        let got = Rc::new(Cell::new(None));
        let g = got.clone();
        let main_thread = std::thread::current().id();
        bridge.call(async { 7u32 }, move |v| {
            assert_eq!(std::thread::current().id(), main_thread);
            g.set(Some(v));
        });
        let start = std::time::Instant::now();
        while got.get().is_none() {
            assert!(start.elapsed().as_secs() < 5, "callback never fired");
            ctx.iteration(false);
        }
        assert_eq!(got.get(), Some(7));
    }
}
