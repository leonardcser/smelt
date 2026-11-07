use std::time::{Duration, Instant};

/// Elapsed timer that can be frozen while user-facing blocking UI is open.
#[derive(Clone, Copy, Debug)]
pub struct PausedTimer {
    start: Instant,
    pause_started: Option<Instant>,
    paused: Duration,
}

impl PausedTimer {
    pub fn new(start: Instant) -> Self {
        Self::with_elapsed(start, Duration::ZERO)
    }

    pub fn with_elapsed(now: Instant, elapsed: Duration) -> Self {
        Self {
            start: now.checked_sub(elapsed).unwrap_or(now),
            pause_started: None,
            paused: Duration::ZERO,
        }
    }

    pub fn elapsed_at(&self, now: Instant) -> Duration {
        let end = self.pause_started.unwrap_or(now);
        end.duration_since(self.start).saturating_sub(self.paused)
    }

    pub fn is_paused(&self) -> bool {
        self.pause_started.is_some()
    }

    pub fn pause(&mut self, now: Instant) {
        if self.pause_started.is_none() {
            self.pause_started = Some(now);
        }
    }

    pub fn resume(&mut self, now: Instant) {
        if let Some(started) = self.pause_started.take() {
            self.paused += now.duration_since(started);
        }
    }
}
