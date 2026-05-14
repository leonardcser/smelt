use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// Lightweight cancellation token backed by an atomic bool + Notify.
/// Drop-in replacement for `tokio_util::sync::CancellationToken`.
#[derive(Clone)]
pub(crate) struct CancellationToken {
    inner: Arc<Inner>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    pub(crate) fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        // Create the Notified future *before* checking the flag so that a
        // cancel() between the check and the await is not lost.
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn new_token_is_not_cancelled() {
        let t = CancellationToken::new();
        assert!(!t.is_cancelled());
    }

    #[test]
    fn default_token_is_not_cancelled() {
        let t = CancellationToken::default();
        assert!(!t.is_cancelled());
    }

    #[test]
    fn cancel_sets_is_cancelled() {
        let t = CancellationToken::new();
        t.cancel();
        assert!(t.is_cancelled());
    }

    #[test]
    fn clone_shares_cancellation_state() {
        let a = CancellationToken::new();
        let b = a.clone();
        a.cancel();
        assert!(b.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_returns_immediately_if_already_cancelled() {
        let t = CancellationToken::new();
        t.cancel();
        tokio::time::timeout(Duration::from_millis(50), t.cancelled())
            .await
            .expect("cancelled() did not return for an already-cancelled token");
    }

    #[tokio::test]
    async fn cancelled_returns_when_cancel_called_from_another_task() {
        let t = CancellationToken::new();
        let t2 = t.clone();
        let waiter = tokio::spawn(async move {
            t2.cancelled().await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        t.cancel();
        tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("waiter did not finish")
            .expect("waiter panicked");
    }

    #[tokio::test]
    async fn cancelled_does_not_return_until_cancelled() {
        let t = CancellationToken::new();
        let res = tokio::time::timeout(Duration::from_millis(50), t.cancelled()).await;
        assert!(
            res.is_err(),
            "cancelled() returned before cancel() was called"
        );
    }
}
