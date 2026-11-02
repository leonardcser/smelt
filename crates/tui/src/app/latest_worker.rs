//! Single-reader workers with one replaceable pending request. Storage and
//! result validation stay with each worker, not with the mailbox.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

struct State<R> {
    pending: Option<(u64, R)>,
    stopped: bool,
}

pub(super) struct Mailbox<R> {
    state: Mutex<State<R>>,
    changed: Condvar,
    generation: AtomicU64,
    #[cfg(test)]
    delay_ms: AtomicU64,
}

pub(super) struct RequestToken<'a> {
    pub(super) generation: u64,
    latest: &'a AtomicU64,
}

impl RequestToken<'_> {
    pub(super) fn cancelled(&self) -> bool {
        self.latest.load(Ordering::Acquire) != self.generation
    }
}

impl<R> Mailbox<R> {
    pub(super) fn recv(&self) -> Option<(RequestToken<'_>, R)> {
        loop {
            let (generation, request) = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                while state.pending.is_none() && !state.stopped {
                    state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                if state.stopped {
                    return None;
                }
                state.pending.take().expect("pending worker request")
            };
            let token = RequestToken {
                generation,
                latest: &self.generation,
            };
            #[cfg(test)]
            {
                let mut remaining = self.delay_ms.load(Ordering::Acquire);
                while remaining > 0 && !token.cancelled() {
                    let sleep_ms = remaining.min(5);
                    thread::sleep(std::time::Duration::from_millis(sleep_ms));
                    remaining -= sleep_ms;
                }
            }
            if !token.cancelled() {
                return Some((token, request));
            }
        }
    }

    fn clear(&self, stopped: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.generation.fetch_add(1, Ordering::AcqRel);
        state.pending = None;
        state.stopped |= stopped;
        self.changed.notify_one();
    }
}

pub(super) struct LatestWorker<R> {
    mailbox: Arc<Mailbox<R>>,
    thread: Option<JoinHandle<()>>,
}

impl<R: Send + 'static> LatestWorker<R> {
    pub(super) fn start(name: &str, run: impl FnOnce(Arc<Mailbox<R>>) + Send + 'static) -> Self {
        let mailbox = Arc::new(Mailbox {
            state: Mutex::new(State {
                pending: None,
                stopped: false,
            }),
            changed: Condvar::new(),
            generation: AtomicU64::new(0),
            #[cfg(test)]
            delay_ms: AtomicU64::new(0),
        });
        let reader = Arc::clone(&mailbox);
        let thread = thread::Builder::new()
            .name(name.into())
            .spawn(move || run(reader))
            .expect("failed to spawn latest-request worker");
        Self {
            mailbox,
            thread: Some(thread),
        }
    }

    pub(super) fn request(&self, request: R) {
        let mut state = self.mailbox.state.lock().unwrap_or_else(|e| e.into_inner());
        let generation = self.mailbox.generation.fetch_add(1, Ordering::AcqRel) + 1;
        state.pending = Some((generation, request));
        self.mailbox.changed.notify_one();
    }

    pub(super) fn cancel(&self) {
        self.mailbox.clear(false);
    }

    pub(super) fn is_current(&self, generation: u64) -> bool {
        self.mailbox.generation.load(Ordering::Acquire) == generation
    }

    #[cfg(test)]
    pub(super) fn set_delay(&self, delay: std::time::Duration) {
        self.mailbox.delay_ms.store(
            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
    }
}

impl<R> Drop for LatestWorker<R> {
    fn drop(&mut self) {
        self.mailbox.clear(true);
        if let Some(thread) = self.thread.take() {
            super::join_background_worker(thread);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn replaces_waiting_requests_and_invalidates_active_reads() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = LatestWorker::start("latest-request-test", move |mailbox| {
            release_rx.recv().unwrap();
            let (token, request) = mailbox.recv().unwrap();
            ready_tx.send(request).unwrap();
            release_rx.recv().unwrap();
            ready_tx.send(u32::from(token.cancelled())).unwrap();
        });
        worker.request(10);
        worker.request(20);
        release_tx.send(()).unwrap();
        assert_eq!(ready_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 20);
        worker.cancel();
        release_tx.send(()).unwrap();
        assert_eq!(ready_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
    }

    #[test]
    fn teardown_does_not_wait_for_stalled_storage() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let worker = LatestWorker::start("stalled-read-test", move |mailbox| {
            let (token, ()) = mailbox.recv().unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            assert!(token.cancelled());
            assert!(mailbox.recv().is_none());
            ready_tx.send(()).unwrap();
        });
        worker.request(());
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        thread::spawn(move || {
            drop(worker);
            dropped_tx.send(()).unwrap();
        });
        let dropped = dropped_rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        dropped.unwrap();
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}
