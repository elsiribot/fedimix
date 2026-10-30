//! Tracks running core operations for whoever started them.

use std::collections::{HashMap, VecDeque};

use fedimix_core::{OperationId, OperationState};

/// Final states nobody watched yet are kept this long, for a watcher that
/// registers late; older ones belong to operations nobody asked about.
const UNCLAIMED_LIMIT: usize = 32;

/// An operation's events can overtake the reply that names its id, so states
/// are recorded even before anyone watches, and replayed to a late watcher.
pub struct OperationTable<W> {
    latest: HashMap<OperationId, OperationState>,
    watchers: HashMap<OperationId, W>,
    unclaimed: VecDeque<OperationId>,
}

impl<W: Clone> OperationTable<W> {
    pub fn new() -> Self {
        Self {
            latest: HashMap::new(),
            watchers: HashMap::new(),
            unclaimed: VecDeque::new(),
        }
    }

    /// Start watching `id`. Returns the state to replay to `watcher`, if the
    /// operation already reported one.
    pub fn watch(&mut self, id: OperationId, watcher: W) -> Option<OperationState> {
        match self.latest.get(&id).cloned() {
            Some(state) if state.is_final() => self.latest.remove(&id),
            other => {
                self.watchers.insert(id, watcher);
                other
            }
        }
    }

    /// Record a state; returns the watcher to tell about it.
    pub fn record(&mut self, id: &str, state: &OperationState) -> Option<W> {
        if !state.is_final() {
            self.latest.insert(id.to_string(), state.clone());
            return self.watchers.get(id).cloned();
        }
        self.latest.remove(id);
        let watcher = self.watchers.remove(id);
        if watcher.is_none() {
            self.latest.insert(id.to_string(), state.clone());
            self.unclaimed.push_back(id.to_string());
            if self.unclaimed.len() > UNCLAIMED_LIMIT
                && let Some(oldest) = self.unclaimed.pop_front()
            {
                self.latest.remove(&oldest);
            }
        }
        watcher
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use OperationState::{Broadcasting, Done, Signing};

    fn table() -> OperationTable<u32> {
        OperationTable::new()
    }

    #[test]
    fn a_watcher_gets_each_state_until_the_final_one() {
        let mut t = table();
        assert_eq!(t.watch("a".into(), 7), None);
        assert_eq!(t.record("a", &Signing), Some(7));
        assert_eq!(t.record("a", &Done), Some(7));
        assert_eq!(t.record("a", &Done), None, "the watcher is gone");
    }

    #[test]
    fn a_late_watcher_gets_the_latest_state_replayed() {
        let mut t = table();
        assert_eq!(t.record("a", &Signing), None);
        assert_eq!(t.record("a", &Broadcasting), None);
        assert_eq!(t.watch("a".into(), 7), Some(Broadcasting));
        assert_eq!(t.record("a", &Done), Some(7));
    }

    #[test]
    fn a_watcher_after_the_final_state_is_not_kept() {
        let mut t = table();
        t.record("a", &Done);
        assert_eq!(t.watch("a".into(), 7), Some(Done));
        assert_eq!(t.record("a", &Signing), None);
    }

    #[test]
    fn unclaimed_final_states_are_bounded() {
        let mut t = table();
        for n in 0..=UNCLAIMED_LIMIT {
            t.record(&n.to_string(), &Done);
        }
        assert_eq!(t.latest.len(), UNCLAIMED_LIMIT);
        assert_eq!(t.watch("0".into(), 7), None);
    }
}
