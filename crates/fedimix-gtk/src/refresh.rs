//! Coalesces snapshot refetches.

/// What to do when a fetch finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    /// Show this result. Off for a result older than one already shown.
    pub apply: bool,
    /// Start this fetch: something changed while the last one ran.
    pub restart: Option<u64>,
}

/// At most one fetch runs at a time. Requests arriving meanwhile collapse
/// into a single follow-up, so a burst of events costs at most two fetches.
#[derive(Debug, Default)]
pub struct RefreshGate {
    /// Newest fetch started.
    started: u64,
    /// Newest fetch whose result was shown.
    applied: u64,
    in_flight: bool,
    dirty: bool,
}

impl RefreshGate {
    /// A refresh is wanted. Returns the fetch to start, if one should.
    pub fn request(&mut self) -> Option<u64> {
        if self.in_flight {
            self.dirty = true;
            return None;
        }
        Some(self.start())
    }

    fn start(&mut self) -> u64 {
        self.in_flight = true;
        self.dirty = false;
        self.started += 1;
        self.started
    }

    /// Fetch `seq` finished, successfully or not.
    pub fn finished(&mut self, seq: u64) -> Finished {
        if seq <= self.applied || seq != self.started {
            return Finished {
                apply: false,
                restart: None,
            };
        }
        self.applied = seq;
        self.in_flight = false;
        Finished {
            apply: true,
            restart: self.dirty.then(|| self.start()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_request_runs_one_fetch() {
        let mut g = RefreshGate::default();
        let seq = g.request().expect("idle gate starts a fetch");
        assert_eq!(
            g.finished(seq),
            Finished {
                apply: true,
                restart: None
            }
        );
    }

    #[test]
    fn a_burst_coalesces_into_one_follow_up() {
        let mut g = RefreshGate::default();
        let first = g.request().unwrap();
        for _ in 0..100 {
            assert_eq!(g.request(), None);
        }
        let done = g.finished(first);
        assert!(done.apply, "still newer than what is on screen");
        let second = done.restart.expect("events arrived during the fetch");
        assert!(second > first);
        assert_eq!(
            g.finished(second),
            Finished {
                apply: true,
                restart: None
            }
        );
    }

    #[test]
    fn requests_during_the_follow_up_get_another_one() {
        let mut g = RefreshGate::default();
        let first = g.request().unwrap();
        g.request();
        let second = g.finished(first).restart.unwrap();
        g.request();
        assert!(g.finished(second).restart.is_some());
    }

    #[test]
    fn idle_again_after_the_last_fetch() {
        let mut g = RefreshGate::default();
        let seq = g.request().unwrap();
        g.finished(seq);
        assert!(g.request().is_some());
    }

    #[test]
    fn a_stale_result_is_dropped() {
        let mut g = RefreshGate::default();
        let first = g.request().unwrap();
        g.request();
        let second = g.finished(first).restart.unwrap();
        assert!(g.finished(second).apply);
        // A reply for an older fetch must not overwrite newer snapshots, and
        // must not be mistaken for the end of a fetch in flight.
        let third = g.request().unwrap();
        assert_eq!(
            g.finished(first),
            Finished {
                apply: false,
                restart: None
            }
        );
        assert_eq!(g.request(), None, "third is still in flight");
        assert!(g.finished(third).apply);
    }
}
