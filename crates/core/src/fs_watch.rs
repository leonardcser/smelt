//! Change-only watcher signals. Detailed Lua subscriptions bypass coalescing.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub enum Signal {
    Changed,
    Shutdown,
}

/// Wait for a change followed by `quiet` without another change. Shutdown or
/// disconnection discards the pending batch and stops the worker immediately.
pub fn wait_for_change(rx: &Receiver<Signal>, quiet: Duration) -> bool {
    if !matches!(rx.recv(), Ok(Signal::Changed)) {
        return false;
    }
    loop {
        match rx.recv_timeout(quiet) {
            Ok(Signal::Changed) => {}
            Err(RecvTimeoutError::Timeout) => return true,
            Ok(Signal::Shutdown) | Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn batches_changes_and_discards_pending_work_on_shutdown() {
        let (tx, rx) = mpsc::channel();
        for _ in 0..3 {
            tx.send(Signal::Changed).unwrap();
        }
        assert!(wait_for_change(&rx, Duration::ZERO));
        tx.send(Signal::Changed).unwrap();
        tx.send(Signal::Shutdown).unwrap();
        assert!(!wait_for_change(&rx, Duration::ZERO));
    }

    #[test]
    fn disconnection_stops_idle_and_pending_batches() {
        for pending in [false, true] {
            let (tx, rx) = mpsc::channel();
            if pending {
                tx.send(Signal::Changed).unwrap();
            }
            drop(tx);
            assert!(!wait_for_change(&rx, Duration::ZERO));
        }
    }
}
