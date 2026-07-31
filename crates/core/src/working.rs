//! Status-bar turn state: a live animating turn and an archived last-turn
//! result. Separate slots prevent the render loop from mistaking a completed
//! turn for an ongoing animation (which would pin CPU at 60 Hz).

use crate::paused_timer::PausedTimer;
use engine::clock::Clock;
use protocol::TurnMeta;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Phase of the currently-running turn. The spinner animates from the
/// live turn's elapsed timer and the phase; `Retrying` additionally carries
/// a delay and attempt counter for the countdown display.
#[derive(Clone, Copy, PartialEq)]
pub enum TurnPhase {
    Working,
    Compacting,
    Retrying { delay: Duration, attempt: u32 },
}

/// Public, plugin-facing snapshot of the overall work state. Resolved by
/// the tui layer from the engine-side `WorkingState` and the per-app
/// busy-token stack: `Idle` / `Busy` / `Done` / `Interrupted` are layered
/// on top of `engine_state` in the cell publisher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    Idle,
    Working,
    Retrying,
    Paused,
    Busy,
    Done,
    Interrupted,
}

impl WorkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Retrying => "retrying",
            Self::Paused => "paused",
            Self::Busy => "busy",
            Self::Done => "done",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Outcome of a completed turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Done,
    Cancelled,
    Errored,
}

impl TurnOutcome {
    pub fn is_interrupted(self) -> bool {
        !matches!(self, Self::Done)
    }
}

/// A turn that is currently running.
struct LiveTurn {
    phase: TurnPhase,
    timer: PausedTimer,
    /// Absolute time at which a `Retrying` phase ends. `None` for
    /// other phases.
    retry_deadline: Option<Instant>,
    tps_samples: Vec<f64>,
    last_spinner_frame: usize,
}

impl LiveTurn {
    fn effective_elapsed(&self, now: Instant) -> Duration {
        self.timer.elapsed_at(now)
    }
}

/// Archived metadata from the last completed turn. Outcome and elapsed are
/// shown while idle; token speed is also mirrored into `last_reported_tps` so
/// the statusline can keep showing it across turn boundaries.
struct LastTurn {
    outcome: TurnOutcome,
    elapsed: Duration,
    avg_tps: Option<f64>,
}

pub struct WorkingState {
    live: Option<LiveTurn>,
    last: Option<LastTurn>,
    /// Most recent non-background token speed reported by the engine.
    last_reported_tps: Option<f64>,
    clock: Arc<dyn Clock>,
}

impl WorkingState {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            live: None,
            last: None,
            last_reported_tps: None,
            clock,
        }
    }

    /// Start a new live turn, or update the phase of the currently-
    /// running one (keeps elapsed time and accumulated `tps_samples`).
    pub fn begin(&mut self, phase: TurnPhase) {
        let now = self.clock.instant_now();
        let retry_deadline = retry_deadline_for(phase, now);
        match self.live.as_mut() {
            Some(live) => {
                live.phase = phase;
                live.retry_deadline = retry_deadline;
            }
            None => {
                self.live = Some(LiveTurn {
                    phase,
                    timer: PausedTimer::new(now),
                    retry_deadline,
                    tps_samples: Vec::new(),
                    last_spinner_frame: usize::MAX,
                });
                self.last = None;
            }
        }
    }

    /// Start a new live turn by carrying forward the archived elapsed time from
    /// the most recently completed turn. If a live turn already exists this is
    /// just a phase update, matching `begin`.
    pub fn continue_from_last(&mut self, phase: TurnPhase) {
        let now = self.clock.instant_now();
        let retry_deadline = retry_deadline_for(phase, now);
        match self.live.as_mut() {
            Some(live) => {
                live.phase = phase;
                live.retry_deadline = retry_deadline;
            }
            None => {
                let elapsed = self
                    .last
                    .as_ref()
                    .map_or(Duration::ZERO, |last| last.elapsed);
                self.live = Some(LiveTurn {
                    phase,
                    timer: PausedTimer::with_elapsed(now, elapsed),
                    retry_deadline,
                    tps_samples: Vec::new(),
                    last_spinner_frame: usize::MAX,
                });
                self.last = None;
            }
        }
    }

    /// Archive the live turn's metadata as `last` and clear live.
    pub fn finish(&mut self, outcome: TurnOutcome) -> TurnMeta {
        let now = self.clock.instant_now();
        let previous_tps = self.last_reported_tps;
        let (elapsed, avg_tps) = match self.live.take() {
            Some(live) => (live.effective_elapsed(now), avg(&live.tps_samples)),
            None => (Duration::ZERO, None),
        };
        let display_tps = avg_tps.or(previous_tps);
        if avg_tps.is_some() {
            self.last_reported_tps = avg_tps;
        }
        self.last = Some(LastTurn {
            outcome,
            elapsed,
            avg_tps,
        });
        turn_meta_for(outcome, elapsed, avg_tps, display_tps)
    }

    /// Archive the live turn's metadata and keep its elapsed timer running for
    /// the next queued turn.
    pub fn finish_and_continue(&mut self, outcome: TurnOutcome, phase: TurnPhase) -> TurnMeta {
        let now = self.clock.instant_now();
        let previous_tps = self.last_reported_tps;
        let retry_deadline = retry_deadline_for(phase, now);
        let Some(live) = self.live.as_mut() else {
            debug_assert!(false, "finish_and_continue called without a live turn");
            let meta = self.finish(outcome);
            self.begin(phase);
            return meta;
        };
        let elapsed = live.effective_elapsed(now);
        let avg_tps = avg(&live.tps_samples);
        let display_tps = avg_tps.or(previous_tps);
        if avg_tps.is_some() {
            self.last_reported_tps = avg_tps;
        }
        self.last = Some(LastTurn {
            outcome,
            elapsed,
            avg_tps,
        });
        live.phase = phase;
        live.retry_deadline = retry_deadline;
        live.tps_samples.clear();
        turn_meta_for(outcome, elapsed, avg_tps, display_tps)
    }

    pub fn clear(&mut self) {
        self.live = None;
        self.last = None;
        self.last_reported_tps = None;
    }

    /// Whether the status bar currently displays a frame-by-frame
    /// animation (spinner glyph rotating, elapsed clock ticking).
    /// `false` once a turn has completed - the archived result is
    /// static text.
    pub fn is_animating(&self) -> bool {
        self.live.is_some()
    }

    pub fn is_compacting(&self) -> bool {
        self.live
            .as_ref()
            .is_some_and(|live| matches!(live.phase, TurnPhase::Compacting))
    }

    /// Engine-only view of the live turn. Returns `None` when no live
    /// turn is running - the caller layers the busy stack on top to
    /// decide `Idle` / `Busy` / `Done` / `Interrupted`.
    pub fn engine_state(&self) -> Option<WorkState> {
        let live = self.live.as_ref()?;
        if live.timer.is_paused() {
            return Some(WorkState::Paused);
        }
        let now = self.clock.instant_now();
        Some(match live.phase {
            TurnPhase::Retrying { .. } if retry_remaining_at(live, now).is_some() => {
                WorkState::Retrying
            }
            TurnPhase::Working | TurnPhase::Compacting | TurnPhase::Retrying { .. } => {
                WorkState::Working
            }
        })
    }

    pub fn phase_label(&self) -> Option<&'static str> {
        let live = self.live.as_ref()?;
        if live.timer.is_paused() {
            return Some("paused");
        }
        let now = self.clock.instant_now();
        Some(match live.phase {
            TurnPhase::Working => "working",
            TurnPhase::Compacting => "compacting",
            TurnPhase::Retrying { .. } if retry_remaining_at(live, now).is_some() => "retrying",
            TurnPhase::Retrying { .. } => "working",
        })
    }

    /// Retry countdown for the cell publisher. Returns `(attempt, remaining_ms)`
    /// while a retry backoff still has time remaining, `None` otherwise.
    pub fn retry_info(&self) -> Option<(u32, u64)> {
        let live = self.live.as_ref()?;
        match live.phase {
            TurnPhase::Retrying { attempt, .. } => {
                let remaining = retry_remaining_at(live, self.clock.instant_now())?;
                Some((attempt, remaining.as_millis() as u64))
            }
            _ => None,
        }
    }

    /// Outcome of the archived last turn, when no live turn is running.
    /// Used by the cell publisher to derive `work_state` and `work_outcome`.
    pub fn last_outcome(&self) -> Option<TurnOutcome> {
        if self.live.is_some() {
            return None;
        }
        self.last.as_ref().map(|l| l.outcome)
    }

    pub fn record_tokens_per_sec(&mut self, tps: f64) {
        if let Some(live) = self.live.as_mut() {
            live.tps_samples.push(tps);
            self.last_reported_tps = avg(&live.tps_samples);
        }
    }

    /// Token speed for the status line. While a new turn is running but has
    /// not reported a sample yet, keep showing the most recent observed value
    /// instead of dropping the segment.
    pub fn display_tps(&self) -> Option<f64> {
        if let Some(live) = self.live.as_ref() {
            return avg(&live.tps_samples).or(self.last_reported_tps);
        }
        self.last
            .as_ref()
            .and_then(|last| last.avg_tps)
            .or(self.last_reported_tps)
    }

    /// Elapsed time for the display: paused-aware elapsed for a live turn,
    /// archived `elapsed` otherwise.
    pub fn elapsed(&self) -> Option<Duration> {
        if let Some(live) = self.live.as_ref() {
            Some(live.effective_elapsed(self.clock.instant_now()))
        } else {
            self.last.as_ref().map(|l| l.elapsed)
        }
    }

    /// Toggle the paused state on the live turn (if any). While paused,
    /// elapsed time and the spinner freeze. Idempotent.
    pub fn set_paused(&mut self, paused: bool) {
        let now = self.clock.instant_now();
        let Some(live) = self.live.as_mut() else {
            return;
        };
        if paused {
            live.timer.pause(now);
        } else {
            live.timer.resume(now);
        }
    }

    pub fn last_spinner_frame(&self) -> Option<usize> {
        self.live.as_ref().map(|l| l.last_spinner_frame)
    }

    pub fn set_last_spinner_frame(&mut self, frame: usize) {
        if let Some(live) = self.live.as_mut() {
            live.last_spinner_frame = frame;
        }
    }

    pub fn turn_meta(&self) -> Option<TurnMeta> {
        if let Some(live) = self.live.as_ref() {
            let avg_tps = avg(&live.tps_samples);
            return Some(turn_meta_for(
                TurnOutcome::Done,
                live.effective_elapsed(self.clock.instant_now()),
                avg_tps,
                avg_tps.or(self.last_reported_tps),
            ));
        }
        self.last.as_ref().map(|last| {
            turn_meta_for(
                last.outcome,
                last.elapsed,
                last.avg_tps,
                last.avg_tps.or(self.last_reported_tps),
            )
        })
    }

    pub fn restore_from_turn_meta(&mut self, meta: &TurnMeta) {
        self.live = None;
        self.last_reported_tps = meta.display_tps.or(meta.avg_tps);
        self.last = Some(LastTurn {
            outcome: if meta.interrupted {
                TurnOutcome::Cancelled
            } else {
                TurnOutcome::Done
            },
            elapsed: Duration::from_millis(meta.elapsed_ms),
            avg_tps: meta.avg_tps,
        });
    }
}

fn turn_meta_for(
    outcome: TurnOutcome,
    elapsed: Duration,
    avg_tps: Option<f64>,
    display_tps: Option<f64>,
) -> TurnMeta {
    TurnMeta {
        elapsed_ms: elapsed.as_millis() as u64,
        avg_tps,
        display_tps,
        interrupted: outcome.is_interrupted(),
    }
}

fn retry_deadline_for(phase: TurnPhase, now: Instant) -> Option<Instant> {
    match phase {
        TurnPhase::Retrying { delay, .. } => Some(now + delay),
        _ => None,
    }
}

fn retry_remaining_at(live: &LiveTurn, now: Instant) -> Option<Duration> {
    let TurnPhase::Retrying { delay, .. } = live.phase else {
        return None;
    };
    let remaining = live
        .retry_deadline
        .map(|t| t.saturating_duration_since(now))
        .unwrap_or(delay);
    (!remaining.is_zero()).then_some(remaining)
}

fn avg(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    Some(samples.iter().sum::<f64>() / samples.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::clock::VirtualClock;
    use std::time::SystemTime;

    /// Build a `WorkingState` whose time advances only via `clock.advance`.
    /// Returning the clock lets each test drive it directly - the determinism
    /// property under test is "tick the clock, observe the state move."
    fn fixture() -> (Arc<VirtualClock>, WorkingState) {
        let clock = Arc::new(VirtualClock::new(Instant::now(), SystemTime::now()));
        let state = WorkingState::new(Arc::clone(&clock) as Arc<dyn Clock>);
        (clock, state)
    }

    #[test]
    fn avg_returns_none_for_empty_samples() {
        assert_eq!(avg(&[]), None);
    }

    #[test]
    fn avg_computes_arithmetic_mean() {
        assert_eq!(avg(&[1.0, 2.0, 3.0]), Some(2.0));
        assert_eq!(avg(&[5.0]), Some(5.0));
    }

    #[test]
    fn new_starts_idle_and_not_animating() {
        let (_clock, s) = fixture();
        assert!(!s.is_animating());
        assert_eq!(s.elapsed(), None);
        assert!(s.turn_meta().is_none());
        assert!(s.engine_state().is_none());
    }

    #[test]
    fn begin_working_marks_state_animating() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        assert!(s.is_animating());
        assert!(s.elapsed().is_some());
        assert_eq!(s.engine_state(), Some(WorkState::Working));
    }

    #[test]
    fn compacting_is_live_working_without_resetting_elapsed() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        clock.advance(Duration::from_millis(500));
        s.begin(TurnPhase::Compacting);

        assert_eq!(s.engine_state(), Some(WorkState::Working));
        assert_eq!(s.phase_label(), Some("compacting"));
        assert_eq!(s.elapsed(), Some(Duration::from_millis(500)));
    }

    #[test]
    fn begin_clears_archived_last_turn() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        clock.advance(Duration::from_millis(5));
        s.finish(TurnOutcome::Done);
        assert!(s.turn_meta().is_some());
        clock.advance(Duration::from_millis(5));
        s.begin(TurnPhase::Working);
        let meta = s.turn_meta().unwrap();
        assert!(!meta.interrupted);
        assert!(meta.avg_tps.is_none());
    }

    #[test]
    fn continue_from_last_carries_elapsed_without_counting_idle_gap() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        clock.advance(Duration::from_millis(750));
        s.finish(TurnOutcome::Done);
        assert_eq!(s.elapsed(), Some(Duration::from_millis(750)));

        clock.advance(Duration::from_millis(1200));
        s.continue_from_last(TurnPhase::Working);
        assert_eq!(s.elapsed(), Some(Duration::from_millis(750)));

        clock.advance(Duration::from_millis(250));
        assert_eq!(s.elapsed(), Some(Duration::from_millis(1000)));
    }

    #[test]
    fn finish_archives_outcome_and_clears_live() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.record_tokens_per_sec(10.0);
        s.record_tokens_per_sec(30.0);
        clock.advance(Duration::from_millis(5));
        s.finish(TurnOutcome::Done);
        assert!(!s.is_animating());
        let meta = s.turn_meta().unwrap();
        assert!(!meta.interrupted);
        assert_eq!(meta.avg_tps, Some(20.0));
    }

    #[test]
    fn finish_cancelled_sets_interrupted_flag() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.finish(TurnOutcome::Cancelled);
        let meta = s.turn_meta().unwrap();
        assert!(meta.interrupted);
    }

    #[test]
    fn finish_without_live_archives_zero_elapsed() {
        let (_clock, mut s) = fixture();
        s.finish(TurnOutcome::Done);
        let meta = s.turn_meta().unwrap();
        assert_eq!(meta.elapsed_ms, 0);
        assert!(meta.avg_tps.is_none());
    }

    #[test]
    fn elapsed_reflects_advanced_clock_for_live_turn() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        assert_eq!(s.elapsed(), Some(Duration::ZERO));
        clock.advance(Duration::from_millis(750));
        assert_eq!(s.elapsed(), Some(Duration::from_millis(750)));
    }

    #[test]
    fn clear_drops_both_live_and_last() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.record_tokens_per_sec(10.0);
        s.finish(TurnOutcome::Done);
        s.clear();
        assert!(!s.is_animating());
        assert!(s.turn_meta().is_none());
        assert!(s.elapsed().is_none());
        assert!(s.display_tps().is_none());
    }

    #[test]
    fn record_tokens_per_sec_only_applies_while_live() {
        let (_clock, mut s) = fixture();
        s.record_tokens_per_sec(99.0); // dropped - no live turn
        s.begin(TurnPhase::Working);
        s.record_tokens_per_sec(1.0);
        s.record_tokens_per_sec(3.0);
        let meta = s.turn_meta().unwrap();
        assert_eq!(meta.avg_tps, Some(2.0));
        assert_eq!(s.display_tps(), Some(2.0));
    }

    #[test]
    fn display_tps_survives_new_turn_until_new_sample() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.record_tokens_per_sec(10.0);
        s.record_tokens_per_sec(30.0);
        s.finish(TurnOutcome::Done);
        assert_eq!(s.display_tps(), Some(20.0));

        s.begin(TurnPhase::Working);
        assert_eq!(s.display_tps(), Some(20.0));
        assert!(s.turn_meta().unwrap().avg_tps.is_none());

        s.record_tokens_per_sec(50.0);
        assert_eq!(s.display_tps(), Some(50.0));
    }

    #[test]
    fn display_tps_survives_turn_without_samples() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.record_tokens_per_sec(12.0);
        s.finish(TurnOutcome::Done);

        s.begin(TurnPhase::Working);
        let meta = s.finish(TurnOutcome::Done);
        assert!(meta.avg_tps.is_none());
        assert_eq!(meta.display_tps, Some(12.0));
        assert_eq!(s.display_tps(), Some(12.0));
    }

    #[test]
    fn restore_from_turn_meta_uses_display_tps_snapshot() {
        let (_clock, mut s) = fixture();
        let meta = TurnMeta {
            elapsed_ms: 200,
            avg_tps: None,
            display_tps: Some(18.0),
            interrupted: false,
        };

        s.restore_from_turn_meta(&meta);

        assert_eq!(s.turn_meta().unwrap().avg_tps, None);
        assert_eq!(s.display_tps(), Some(18.0));
    }

    #[test]
    fn set_paused_freezes_elapsed_clock() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        clock.advance(Duration::from_millis(50));
        assert_eq!(s.elapsed(), Some(Duration::from_millis(50)));
        s.set_paused(true);
        assert_eq!(s.engine_state(), Some(WorkState::Paused));
        clock.advance(Duration::from_millis(100));
        // Elapsed is frozen while paused.
        assert_eq!(s.elapsed(), Some(Duration::from_millis(50)));
        s.set_paused(false);
        assert_eq!(s.engine_state(), Some(WorkState::Working));
        clock.advance(Duration::from_millis(10));
        assert_eq!(s.elapsed(), Some(Duration::from_millis(60)));
    }

    #[test]
    fn set_paused_is_idempotent() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.set_paused(true);
        clock.advance(Duration::from_millis(5));
        s.set_paused(true); // no-op
        assert_eq!(s.engine_state(), Some(WorkState::Paused));
        clock.advance(Duration::from_millis(5));
        s.set_paused(false);
        clock.advance(Duration::from_millis(10));
        s.set_paused(false); // no-op
        assert_eq!(s.engine_state(), Some(WorkState::Working));
    }

    #[test]
    fn set_paused_without_live_turn_is_noop() {
        let (_clock, mut s) = fixture();
        s.set_paused(true);
        s.set_paused(false);
        assert!(!s.is_animating());
    }

    #[test]
    fn pause_resume_excludes_paused_window_from_elapsed() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        clock.advance(Duration::from_millis(100));
        s.set_paused(true);
        clock.advance(Duration::from_millis(400));
        // Frozen elapsed while paused.
        assert_eq!(s.elapsed(), Some(Duration::from_millis(100)));
        s.set_paused(false);
        clock.advance(Duration::from_millis(200));
        // After resume, elapsed = 100ms pre-pause + 200ms post-resume.
        assert_eq!(s.elapsed(), Some(Duration::from_millis(300)));
    }

    #[test]
    fn last_spinner_frame_round_trips_while_live() {
        let (_clock, mut s) = fixture();
        assert!(s.last_spinner_frame().is_none());
        s.begin(TurnPhase::Working);
        assert_eq!(s.last_spinner_frame(), Some(usize::MAX));
        s.set_last_spinner_frame(7);
        assert_eq!(s.last_spinner_frame(), Some(7));
    }

    #[test]
    fn set_last_spinner_frame_without_live_is_noop() {
        let (_clock, mut s) = fixture();
        s.set_last_spinner_frame(5);
        assert!(s.last_spinner_frame().is_none());
    }

    #[test]
    fn restore_from_turn_meta_archives_done_outcome() {
        let (_clock, mut s) = fixture();
        let meta = TurnMeta {
            elapsed_ms: 1500,
            avg_tps: Some(42.0),
            display_tps: Some(42.0),
            interrupted: false,
        };
        s.restore_from_turn_meta(&meta);
        assert!(!s.is_animating());
        let round = s.turn_meta().unwrap();
        assert_eq!(round.elapsed_ms, 1500);
        assert_eq!(round.avg_tps, Some(42.0));
        assert_eq!(s.display_tps(), Some(42.0));
        assert!(!round.interrupted);
    }

    #[test]
    fn restore_from_turn_meta_archives_interrupted_outcome() {
        let (_clock, mut s) = fixture();
        let meta = TurnMeta {
            elapsed_ms: 200,
            avg_tps: None,
            display_tps: None,
            interrupted: true,
        };
        s.restore_from_turn_meta(&meta);
        let round = s.turn_meta().unwrap();
        assert!(round.interrupted);
    }

    #[test]
    fn restore_from_turn_meta_discards_existing_live_turn() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        let meta = TurnMeta {
            elapsed_ms: 0,
            avg_tps: None,
            display_tps: None,
            interrupted: false,
        };
        s.restore_from_turn_meta(&meta);
        assert!(!s.is_animating());
    }

    #[test]
    fn engine_state_idle_returns_none() {
        let (_clock, s) = fixture();
        assert!(s.engine_state().is_none());
    }

    #[test]
    fn engine_state_working_after_begin() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        assert_eq!(s.engine_state(), Some(WorkState::Working));
    }

    #[test]
    fn engine_state_retrying_after_begin_retrying() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Retrying {
            delay: Duration::from_secs(2),
            attempt: 1,
        });
        assert_eq!(s.engine_state(), Some(WorkState::Retrying));
    }

    #[test]
    fn engine_state_paused_when_timer_paused() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.set_paused(true);
        assert_eq!(s.engine_state(), Some(WorkState::Paused));
        s.set_paused(false);
        assert_eq!(s.engine_state(), Some(WorkState::Working));
    }

    #[test]
    fn engine_state_idle_after_finish() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.finish(TurnOutcome::Done);
        // `Done` is layered by the tui resolver on top of the busy stack;
        // `engine_state` itself only describes the live turn.
        assert!(s.engine_state().is_none());
        assert_eq!(s.last_outcome(), Some(TurnOutcome::Done));
    }

    #[test]
    fn retry_info_reports_attempt_and_remaining() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Retrying {
            delay: Duration::from_secs(5),
            attempt: 3,
        });
        let (attempt, remaining_ms) = s.retry_info().expect("retry info");
        assert_eq!(attempt, 3);
        assert_eq!(remaining_ms, 5000);
        clock.advance(Duration::from_secs(2));
        let (_, remaining_ms) = s.retry_info().expect("retry info");
        assert_eq!(remaining_ms, 3000);
    }

    #[test]
    fn retry_countdown_expires_back_to_working() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Retrying {
            delay: Duration::from_millis(500),
            attempt: 1,
        });

        assert_eq!(s.engine_state(), Some(WorkState::Retrying));
        assert_eq!(s.phase_label(), Some("retrying"));
        assert!(s.retry_info().is_some());

        clock.advance(Duration::from_millis(500));

        assert_eq!(s.engine_state(), Some(WorkState::Working));
        assert_eq!(s.phase_label(), Some("working"));
        assert!(s.retry_info().is_none());
    }

    #[test]
    fn retry_info_none_when_working() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        assert!(s.retry_info().is_none());
    }

    #[test]
    fn last_outcome_cancelled_after_finish_cancelled() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.finish(TurnOutcome::Cancelled);
        assert_eq!(s.last_outcome(), Some(TurnOutcome::Cancelled));
    }
}
