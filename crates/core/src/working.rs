//! Status-bar turn state: a live animating turn and an archived last-turn
//! result. Separate slots prevent the render loop from mistaking a completed
//! turn for an ongoing animation (which would pin CPU at 60 Hz).

use crate::content::SPINNER_FRAMES;
use crate::utils::format_duration;
use engine::clock::Clock;
use protocol::TurnMeta;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Phase of the currently-running turn. The spinner animates based on
/// `since` and the phase; `Retrying` additionally carries a delay and
/// attempt counter for the countdown display.
#[derive(Clone, Copy, PartialEq)]
pub enum TurnPhase {
    Working,
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
    Interrupted,
}

/// A turn that is currently running.
struct LiveTurn {
    phase: TurnPhase,
    /// Anchor for elapsed math. Shifted forward by the pause duration
    /// on resume, so `since.elapsed()` is the correct paused-aware
    /// elapsed whenever `pause_started` is `None`.
    since: Instant,
    /// Absolute time at which a `Retrying` phase ends. `None` for
    /// other phases.
    retry_deadline: Option<Instant>,
    tps_samples: Vec<f64>,
    last_spinner_frame: usize,
    /// Some(t) while paused (blocking dialog up). Timer + spinner
    /// freeze; `since` gets shifted forward by `t.elapsed()` on resume.
    pause_started: Option<Instant>,
}

impl LiveTurn {
    /// Elapsed time the user sees: frozen at the moment of pause while
    /// `pause_started` is set, otherwise `now - since` (paused-aware
    /// because resume shifts `since` forward by the pause duration).
    fn effective_elapsed(&self, now: Instant) -> Duration {
        match self.pause_started {
            Some(t) => t.duration_since(self.since),
            None => now.duration_since(self.since),
        }
    }
}

/// Archived metadata from the last completed turn. Shown in the
/// status bar until the next `begin()`.
struct LastTurn {
    outcome: TurnOutcome,
    elapsed: Duration,
    avg_tps: Option<f64>,
}

pub struct WorkingState {
    live: Option<LiveTurn>,
    last: Option<LastTurn>,
    clock: Arc<dyn Clock>,
}

impl WorkingState {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            live: None,
            last: None,
            clock,
        }
    }

    /// Start a new live turn, or update the phase of the currently-
    /// running one (keeps `since` and accumulated `tps_samples`).
    pub fn begin(&mut self, phase: TurnPhase) {
        let now = self.clock.instant_now();
        let retry_deadline = match phase {
            TurnPhase::Retrying { delay, .. } => Some(now + delay),
            _ => None,
        };
        match self.live.as_mut() {
            Some(live) => {
                live.phase = phase;
                live.retry_deadline = retry_deadline;
            }
            None => {
                self.live = Some(LiveTurn {
                    phase,
                    since: now,
                    retry_deadline,
                    tps_samples: Vec::new(),
                    last_spinner_frame: usize::MAX,
                    pause_started: None,
                });
                self.last = None;
            }
        }
    }

    /// Archive the live turn's metadata as `last` and clear live.
    pub fn finish(&mut self, outcome: TurnOutcome) {
        let now = self.clock.instant_now();
        let (elapsed, avg_tps) = match self.live.take() {
            Some(live) => (live.effective_elapsed(now), avg(&live.tps_samples)),
            None => (Duration::ZERO, None),
        };
        self.last = Some(LastTurn {
            outcome,
            elapsed,
            avg_tps,
        });
    }

    pub fn clear(&mut self) {
        self.live = None;
        self.last = None;
    }

    /// Whether the status bar currently displays a frame-by-frame
    /// animation (spinner glyph rotating, elapsed clock ticking).
    /// `false` once a turn has completed — the archived result is
    /// static text.
    pub fn is_animating(&self) -> bool {
        self.live.is_some()
    }

    /// Engine-only view of the live turn. Returns `None` when no live
    /// turn is running — the caller layers the busy stack on top to
    /// decide `Idle` / `Busy` / `Done` / `Interrupted`.
    pub fn engine_state(&self) -> Option<WorkState> {
        let live = self.live.as_ref()?;
        if live.pause_started.is_some() {
            return Some(WorkState::Paused);
        }
        Some(match live.phase {
            TurnPhase::Retrying { .. } => WorkState::Retrying,
            TurnPhase::Working => WorkState::Working,
        })
    }

    /// Retry countdown for the cell publisher. Returns `(attempt, remaining_ms)`
    /// while the live turn is in `Retrying` phase, `None` otherwise.
    pub fn retry_info(&self) -> Option<(u32, u64)> {
        let live = self.live.as_ref()?;
        match live.phase {
            TurnPhase::Retrying { delay, attempt } => {
                let now = self.clock.instant_now();
                let remaining = live
                    .retry_deadline
                    .map(|t| t.saturating_duration_since(now))
                    .unwrap_or(delay)
                    .as_millis() as u64;
                Some((attempt, remaining))
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
        }
    }

    /// Elapsed time for the display — `since` for a live turn,
    /// archived `elapsed` otherwise. Live elapsed excludes time
    /// during which a blocking dialog paused the turn.
    pub fn elapsed(&self) -> Option<Duration> {
        if let Some(live) = self.live.as_ref() {
            Some(live.effective_elapsed(self.clock.instant_now()))
        } else {
            self.last.as_ref().map(|l| l.elapsed)
        }
    }

    /// Toggle the paused state on the live turn (if any). While paused,
    /// `effective_elapsed` and the spinner freeze. On resume, `since`
    /// is shifted forward by the pause duration so subsequent elapsed
    /// reads are still correct. Idempotent.
    pub fn set_paused(&mut self, paused: bool) {
        let now = self.clock.instant_now();
        let Some(live) = self.live.as_mut() else {
            return;
        };
        match (paused, live.pause_started) {
            (true, None) => live.pause_started = Some(now),
            (false, Some(t)) => {
                live.since += now.duration_since(t);
                live.pause_started = None;
            }
            _ => {}
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
            return Some(TurnMeta {
                elapsed_ms: live.effective_elapsed(self.clock.instant_now()).as_millis() as u64,
                avg_tps: avg(&live.tps_samples),
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            });
        }
        self.last.as_ref().map(|last| TurnMeta {
            elapsed_ms: last.elapsed.as_millis() as u64,
            avg_tps: last.avg_tps,
            interrupted: matches!(last.outcome, TurnOutcome::Interrupted),
            tool_elapsed: std::collections::HashMap::new(),
        })
    }

    pub fn restore_from_turn_meta(&mut self, meta: &TurnMeta) {
        self.live = None;
        self.last = Some(LastTurn {
            outcome: if meta.interrupted {
                TurnOutcome::Interrupted
            } else {
                TurnOutcome::Done
            },
            elapsed: Duration::from_millis(meta.elapsed_ms),
            avg_tps: meta.avg_tps,
        });
    }

    /// Spinner glyph for the active live turn, or `None` when
    /// nothing is animating *or* the turn is paused by a blocking
    /// dialog. The status bar uses `None` to drop the spinner span
    /// entirely while paused — the label still renders.
    pub fn spinner_char(&self) -> Option<&'static str> {
        let live = self.live.as_ref()?;
        if live.pause_started.is_some() {
            return None;
        }
        let elapsed = live.effective_elapsed(self.clock.instant_now());
        Some(SPINNER_FRAMES[crate::content::spinner_frame_index(elapsed)])
    }

    /// Headless-safe throbber data. The caller (statusline composer in
    /// tui) applies theme colours and builds the Lua table.
    ///
    /// `busy_label` overrides the rendered label when set: when a turn
    /// is live it replaces the `"working"` text; when no turn is live
    /// but a busy token is set, the throbber still renders with the
    /// spinner and label so plugins can drive long-running background
    /// work into the status bar. `busy_since` anchors the spinner
    /// frame index for the standalone-busy case.
    pub fn throbber_data(
        &self,
        show_tps: bool,
        busy_label: Option<&str>,
        busy_since: Option<Instant>,
    ) -> Vec<ThrobberItem> {
        let now = self.clock.instant_now();
        let mut out = Vec::new();
        if let Some(live) = self.live.as_ref() {
            let elapsed = live.effective_elapsed(now);
            let idx = crate::content::spinner_frame_index(elapsed);
            match live.phase {
                TurnPhase::Working | TurnPhase::Retrying { .. } => {
                    let is_retry = matches!(live.phase, TurnPhase::Retrying { .. });
                    let label = busy_label.unwrap_or("working");
                    out.push(ThrobberItem {
                        text: format!(" {} {} ", SPINNER_FRAMES[idx], label),
                        bold: true,
                        dim: false,
                        priority: 0,
                        is_muted: is_retry,
                    });
                    out.push(ThrobberItem {
                        text: format!(" {} ", format_duration(elapsed.as_secs())),
                        bold: false,
                        dim: true,
                        priority: 0,
                        is_muted: true,
                    });
                    if show_tps {
                        if let Some(tps) = avg(&live.tps_samples) {
                            out.push(ThrobberItem {
                                text: "·".into(),
                                bold: false,
                                dim: true,
                                priority: 3,
                                is_muted: true,
                            });
                            out.push(ThrobberItem {
                                text: format!(" {:.1} tok/s", tps),
                                bold: false,
                                dim: true,
                                priority: 3,
                                is_muted: true,
                            });
                        }
                    }
                    if let TurnPhase::Retrying { delay, attempt } = live.phase {
                        let remaining = live
                            .retry_deadline
                            .map(|t| t.saturating_duration_since(now))
                            .unwrap_or(delay)
                            .as_secs();
                        out.push(ThrobberItem {
                            text: format!(" (retrying in {}s #{}) ", remaining, attempt),
                            bold: false,
                            dim: true,
                            priority: 0,
                            is_muted: true,
                        });
                    }
                }
            }
        } else if let (Some(label), Some(since)) = (busy_label, busy_since) {
            let elapsed = now.duration_since(since);
            let idx = crate::content::spinner_frame_index(elapsed);
            out.push(ThrobberItem {
                text: format!(" {} {} ", SPINNER_FRAMES[idx], label),
                bold: true,
                dim: false,
                priority: 0,
                is_muted: false,
            });
            out.push(ThrobberItem {
                text: format!(" {} ", format_duration(elapsed.as_secs())),
                bold: false,
                dim: true,
                priority: 0,
                is_muted: true,
            });
        } else if let Some(last) = self.last.as_ref() {
            match last.outcome {
                TurnOutcome::Done => {
                    out.push(ThrobberItem {
                        text: format!(" done {} ", format_duration(last.elapsed.as_secs())),
                        bold: false,
                        dim: true,
                        priority: 0,
                        is_muted: true,
                    });
                    if show_tps {
                        if let Some(tps) = last.avg_tps {
                            out.push(ThrobberItem {
                                text: "·".into(),
                                bold: false,
                                dim: true,
                                priority: 3,
                                is_muted: true,
                            });
                            out.push(ThrobberItem {
                                text: format!(" {:.1} tok/s", tps),
                                bold: false,
                                dim: true,
                                priority: 3,
                                is_muted: true,
                            });
                        }
                    }
                }
                TurnOutcome::Interrupted => {
                    out.push(ThrobberItem {
                        text: " interrupted ".into(),
                        bold: false,
                        dim: true,
                        priority: 0,
                        is_muted: true,
                    });
                }
            }
        }
        out
    }
}

/// A single item in the throbber row.  Colours are applied by the tui
/// composer because `core` must not depend on crossterm.
#[derive(Debug, Default)]
pub struct ThrobberItem {
    pub text: String,
    pub bold: bool,
    pub dim: bool,
    pub priority: u8,
    /// When true the tui composer paints this item with the muted theme
    /// colour; when false it uses the default (Reset) colour.
    pub is_muted: bool,
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
    /// Returning the clock lets each test drive it directly — the determinism
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
        assert!(s.spinner_char().is_none());
        assert!(s.throbber_data(false, None, None).is_empty());
    }

    #[test]
    fn begin_working_marks_state_animating() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        assert!(s.is_animating());
        assert!(s.elapsed().is_some());
        assert!(s.spinner_char().is_some());
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
    fn finish_interrupted_sets_interrupted_flag() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.finish(TurnOutcome::Interrupted);
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
        s.finish(TurnOutcome::Done);
        s.clear();
        assert!(!s.is_animating());
        assert!(s.turn_meta().is_none());
        assert!(s.elapsed().is_none());
    }

    #[test]
    fn record_tokens_per_sec_only_applies_while_live() {
        let (_clock, mut s) = fixture();
        s.record_tokens_per_sec(99.0); // dropped — no live turn
        s.begin(TurnPhase::Working);
        s.record_tokens_per_sec(1.0);
        s.record_tokens_per_sec(3.0);
        let meta = s.turn_meta().unwrap();
        assert_eq!(meta.avg_tps, Some(2.0));
    }

    #[test]
    fn set_paused_freezes_spinner_and_clears_glyph() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        assert!(s.spinner_char().is_some());
        s.set_paused(true);
        assert!(s.spinner_char().is_none());
        s.set_paused(false);
        assert!(s.spinner_char().is_some());
    }

    #[test]
    fn set_paused_is_idempotent() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.set_paused(true);
        clock.advance(Duration::from_millis(5));
        s.set_paused(true); // no-op
        assert!(s.spinner_char().is_none());
        clock.advance(Duration::from_millis(5));
        s.set_paused(false);
        clock.advance(Duration::from_millis(10));
        s.set_paused(false); // no-op
        clock.advance(Duration::from_millis(10));
        assert!(s.spinner_char().is_some());
    }

    #[test]
    fn set_paused_without_live_turn_is_noop() {
        let (_clock, mut s) = fixture();
        s.set_paused(true);
        s.set_paused(false);
        assert!(!s.is_animating());
    }

    #[test]
    fn pause_resume_shifts_since_so_elapsed_excludes_paused_window() {
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
            interrupted: false,
            tool_elapsed: std::collections::HashMap::new(),
        };
        s.restore_from_turn_meta(&meta);
        assert!(!s.is_animating());
        let round = s.turn_meta().unwrap();
        assert_eq!(round.elapsed_ms, 1500);
        assert_eq!(round.avg_tps, Some(42.0));
        assert!(!round.interrupted);
    }

    #[test]
    fn restore_from_turn_meta_archives_interrupted_outcome() {
        let (_clock, mut s) = fixture();
        let meta = TurnMeta {
            elapsed_ms: 200,
            avg_tps: None,
            interrupted: true,
            tool_elapsed: std::collections::HashMap::new(),
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
            interrupted: false,
            tool_elapsed: std::collections::HashMap::new(),
        };
        s.restore_from_turn_meta(&meta);
        assert!(!s.is_animating());
    }

    #[test]
    fn throbber_data_working_emits_label_and_clock() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        let items = s.throbber_data(false, None, None);
        assert!(items.len() >= 2);
        assert!(items[0].text.contains("working"));
        assert!(items[0].bold);
        assert!(!items[0].is_muted);
        assert!(items[1].is_muted);
    }

    #[test]
    fn throbber_data_uses_busy_label_when_live_turn_is_running() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        let items = s.throbber_data(false, Some("compacting"), None);
        assert!(items[0].text.contains("compacting"));
        assert!(!items[0].text.contains("working"));
    }

    #[test]
    fn throbber_data_renders_busy_only_when_no_live_turn() {
        let (_clock, s) = fixture();
        let items = s.throbber_data(false, Some("syncing"), Some(Instant::now()));
        assert!(items.iter().any(|i| i.text.contains("syncing")));
    }

    #[test]
    fn throbber_data_ignores_busy_args_when_idle_and_no_anchor() {
        let (_clock, s) = fixture();
        let items = s.throbber_data(false, Some("syncing"), None);
        assert!(items.is_empty());
    }

    #[test]
    fn throbber_data_includes_tps_when_requested_and_available() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.record_tokens_per_sec(12.5);
        let with_tps = s.throbber_data(true, None, None);
        let with_tps_text: String = with_tps.iter().map(|i| i.text.as_str()).collect();
        assert!(with_tps_text.contains("12.5 tok/s"));
        let without_tps = s.throbber_data(false, None, None);
        let without_tps_text: String = without_tps.iter().map(|i| i.text.as_str()).collect();
        assert!(!without_tps_text.contains("tok/s"));
    }

    #[test]
    fn throbber_data_omits_tps_when_no_samples_recorded() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        let items = s.throbber_data(true, None, None);
        let text: String = items.iter().map(|i| i.text.as_str()).collect();
        assert!(!text.contains("tok/s"));
    }

    #[test]
    fn throbber_data_retrying_includes_attempt_and_countdown() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Retrying {
            delay: Duration::from_secs(5),
            attempt: 2,
        });
        let items = s.throbber_data(false, None, None);
        let text: String = items.iter().map(|i| i.text.as_str()).collect();
        assert!(text.contains("retrying"));
        assert!(text.contains("#2"));
        let working_label = items
            .iter()
            .find(|i| i.text.contains("working"))
            .expect("working label present");
        assert!(working_label.is_muted);
    }

    #[test]
    fn throbber_data_retrying_countdown_drops_as_clock_advances() {
        let (clock, mut s) = fixture();
        s.begin(TurnPhase::Retrying {
            delay: Duration::from_secs(5),
            attempt: 1,
        });
        let early: String = s
            .throbber_data(false, None, None)
            .iter()
            .map(|i| i.text.as_str())
            .collect();
        assert!(early.contains("retrying in 5s"));
        clock.advance(Duration::from_secs(3));
        let later: String = s
            .throbber_data(false, None, None)
            .iter()
            .map(|i| i.text.as_str())
            .collect();
        assert!(later.contains("retrying in 2s"));
    }

    #[test]
    fn throbber_data_done_outcome_emits_done_text_and_optional_tps() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.record_tokens_per_sec(10.0);
        s.finish(TurnOutcome::Done);
        let items = s.throbber_data(true, None, None);
        let text: String = items.iter().map(|i| i.text.as_str()).collect();
        assert!(text.contains("done"));
        assert!(text.contains("10.0 tok/s"));
    }

    #[test]
    fn throbber_data_done_without_tps_omits_tok_per_sec() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.finish(TurnOutcome::Done);
        let items = s.throbber_data(true, None, None);
        let text: String = items.iter().map(|i| i.text.as_str()).collect();
        assert!(text.contains("done"));
        assert!(!text.contains("tok/s"));
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
    fn engine_state_paused_when_pause_started() {
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
    fn retry_info_none_when_working() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        assert!(s.retry_info().is_none());
    }

    #[test]
    fn throbber_data_interrupted_outcome_emits_interrupted_text() {
        let (_clock, mut s) = fixture();
        s.begin(TurnPhase::Working);
        s.finish(TurnOutcome::Interrupted);
        let items = s.throbber_data(true, None, None);
        let text: String = items.iter().map(|i| i.text.as_str()).collect();
        assert!(text.contains("interrupted"));
        assert!(!text.contains("tok/s"));
    }
}
