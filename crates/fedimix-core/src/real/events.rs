//! Keeps the store and the subscribers current: one supervised task per
//! federation connects it and forwards its balance and online changes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::watch;
use tokio::time::Instant;

use super::anonymity::follow_sessions;
use super::{Core, Gate, held, problem_of};
use crate::api::{Amount, WalletKind};
use crate::error::{FedimixError, Result};
use crate::event::{Event, EventStream};
use crate::federation::FederationClient;

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How often a steady connection looks at sends left with no operation.
const RESOLVE_INTERVAL: Duration = Duration::from_secs(60);

/// `None` once closed, so streams end and a late subscriber sees that too.
pub(super) struct Events {
    senders: Mutex<Option<Vec<async_channel::Sender<Event>>>>,
}

impl Default for Events {
    fn default() -> Self {
        Self {
            senders: Mutex::new(Some(Vec::new())),
        }
    }
}

impl Events {
    pub(super) fn subscribe(&self) -> EventStream {
        let (tx, rx) = async_channel::unbounded();
        if let Some(senders) = held(&self.senders).as_mut() {
            senders.push(tx);
        }
        EventStream::new(rx)
    }

    /// Reaches every live subscriber; closed ones are dropped.
    pub(super) fn emit(&self, event: &Event) {
        if let Some(senders) = held(&self.senders).as_mut() {
            senders.retain(|tx| tx.try_send(event.clone()).is_ok());
        }
    }

    pub(super) fn close(&self) {
        held(&self.senders).take();
    }
}

/// Returns when connections are no longer allowed.
pub(super) async fn left_open(gate: &mut watch::Receiver<Gate>) {
    let _ = gate.wait_for(|gate| *gate != Gate::Open).await;
}

fn next_backoff(backoff: Duration) -> Duration {
    (backoff * 2).min(MAX_BACKOFF)
}

/// Keeps one federation connected until the gate stops it, and returns how
/// closing its client went. The connection runs as its own task, so a panic
/// ends only that. The client stays with `Core`, so the panic does not lose it:
/// fedimint closes a dropped client on its own, but nothing could wait for that
/// before opening the database again.
pub(super) async fn supervise(core: Arc<Core>, id: String) -> Result<()> {
    let mut gate = core.gate.subscribe();
    let mut backoff = FIRST_BACKOFF;
    let wake = Arc::clone(&core.state().live.entry(id.clone()).or_default().wake);
    loop {
        let started = Instant::now();
        if *gate.borrow_and_update() == Gate::Open {
            let mut task = tokio::spawn(connect_and_forward(Arc::clone(&core), id.clone()));
            let ended = tokio::select! {
                ended = &mut task => ended,
                () = left_open(&mut gate) => {
                    task.abort();
                    task.await
                }
            };
            if let Err(e) = ended
                && e.is_panic()
            {
                eprintln!("fedimix: the task of federation {id} panicked; restarting it");
            }
        }
        let connected = core
            .state()
            .live
            .get(&id)
            .is_some_and(|l| l.client.is_some());
        let closed = close_client(&core, &id).await;
        let now = *gate.borrow_and_update();
        match now {
            Gate::Stopped => return closed,
            Gate::Proxy => {
                if let Err(e) = closed {
                    eprintln!("fedimix: closing federation {id}: {e}");
                }
                let _ = gate.wait_for(|gate| *gate != Gate::Proxy).await;
                backoff = FIRST_BACKOFF;
                continue;
            }
            Gate::Open => {}
        }
        if let Err(e) = closed {
            eprintln!("fedimix: closing federation {id}: {e}");
        }
        if connected && started.elapsed() >= MAX_BACKOFF {
            backoff = FIRST_BACKOFF;
        }
        tokio::select! {
            () = tokio::time::sleep(backoff) => backoff = next_backoff(backoff),
            () = left_open(&mut gate) => {}
            () = wake.notified() => backoff = FIRST_BACKOFF,
        }
    }
}

/// Marks the federation offline and closes its client, if it has one, in a
/// task of its own to contain a panic.
async fn close_client(core: &Core, id: &str) -> Result<()> {
    let (client, was_online) = {
        let mut state = core.state();
        let live = state.live.entry(id.to_string()).or_default();
        live.notes.clear();
        (live.client.take(), std::mem::take(&mut live.online))
    };
    if was_online {
        announce_online(core, id, false);
    }
    let Some(client) = client else {
        return Ok(());
    };
    tokio::spawn(client.shutdown()).await.unwrap_or_else(|_| {
        Err(FedimixError::Internal(
            "A federation's connection did not close cleanly.".into(),
        ))
    })
}

/// Returns when the client's streams end: a closed client has not ended the
/// federation, so the supervisor connects again. Everything here can stop at any
/// await: a proxy or a shutdown aborts this task, and with it the watches it
/// started, which the next connection takes up from the store.
async fn connect_and_forward(core: Arc<Core>, id: String) {
    let attached = core.state().live.get(&id).and_then(|l| l.client.clone());
    let client = match attached {
        Some(client) => client,
        None => match core.connect(&id).await {
            Ok(client) => {
                let mut state = core.state();
                let live = state.live.entry(id.clone()).or_default();
                live.client = Some(Arc::clone(&client));
                live.last_open_error = None;
                client
            }
            Err(e) => {
                if e != FedimixError::Offline {
                    eprintln!("fedimix: opening federation {id}: {e}");
                }
                // A federation that keeps failing the same way is no news. A
                // front end only looks again on an event, so a change must send one.
                let changed = {
                    let mut state = core.state();
                    let live = state.live.entry(id.clone()).or_default();
                    let before = live.last_open_error.as_ref().and_then(problem_of);
                    let changed = before != problem_of(&e);
                    live.last_open_error = Some(e);
                    changed
                };
                if changed {
                    core.events.emit(&Event::FederationChanged);
                }
                return;
            }
        },
    };
    let mut watches = core.resume_watches(&client, &id);
    watches.spawn(follow_sessions(
        Arc::clone(&core),
        Arc::clone(&client),
        id.clone(),
    ));
    let mut balances = client.balance_changes();
    let mut online = client.online_changes();
    let mut resolve = tokio::time::interval_at(Instant::now() + RESOLVE_INTERVAL, RESOLVE_INTERVAL);
    loop {
        tokio::select! {
            balance = balances.next() => match balance {
                Some(balance) => forward_balance(&core, &client, &id, balance).await,
                None => return,
            },
            status = online.next() => match status {
                Some(status) => {
                    forward_online(&core, &id, status);
                    if status {
                        watches.resolve_unsent(&id);
                    }
                }
                None => return,
            },
            _ = resolve.tick() => {
                let live = core.state().live.get(&id).is_some_and(|live| live.online);
                if live {
                    watches.resolve_unsent(&id);
                }
            }
            () = watches.tend() => {}
        }
    }
}

/// The notes are read here, so that a snapshot never has to ask the client.
async fn forward_balance(
    core: &Core,
    client: &Arc<dyn FederationClient>,
    id: &str,
    balance: Amount,
) {
    let changed = core
        .store()
        .set_federation_balance(id, balance.msat)
        .unwrap_or_else(|e| {
            eprintln!("fedimix: saving the balance of federation {id}: {e}");
            true
        });
    let notes_changed = match client.notes().await {
        Ok(notes) => {
            let mut state = core.state();
            let live = state.live.entry(id.to_string()).or_default();
            let notes_changed = live.notes != notes;
            live.notes = notes;
            notes_changed
        }
        Err(e) => {
            eprintln!("fedimix: reading the notes of federation {id}: {e}");
            false
        }
    };
    if !changed && !notes_changed {
        return;
    }
    core.events.emit(&if core.is_active(id).unwrap_or(false) {
        Event::BalanceChanged {
            wallet: WalletKind::Spending,
            balance,
        }
    } else {
        Event::FederationChanged
    });
}

fn forward_online(core: &Core, id: &str, online: bool) {
    let was_online = {
        let mut state = core.state();
        let live = state.live.entry(id.to_string()).or_default();
        std::mem::replace(&mut live.online, online)
    };
    if was_online == online {
        return;
    }
    if online && let Err(e) = core.store().set_online_at(id, (core.clock)()) {
        eprintln!("fedimix: saving when federation {id} was online: {e}");
    }
    announce_online(core, id, online);
}

fn announce_online(core: &Core, id: &str, online: bool) {
    core.events.emit(&if core.is_active(id).unwrap_or(false) {
        Event::ConnectionChanged { online }
    } else {
        Event::FederationChanged
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_one_second_to_a_minute() {
        let mut backoff = FIRST_BACKOFF;
        let mut seen = Vec::new();
        for _ in 0..8 {
            seen.push(backoff.as_secs());
            backoff = next_backoff(backoff);
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 32, 60, 60]);
    }
}
