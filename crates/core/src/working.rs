//! Turn state for the status bar: a running "live" turn (animating
//! spinner, elapsed clock, tokens-per-second accumulator) and/or the
//! "last" archived turn result displayed until the next turn begins.
//!
//! Splitting these into separate slots — instead of one `throbber`
//! that carries both "animating" and "display-done" semantics — makes
//! it impossible for the render loop to mistake a completed turn for
//! a frame-by-frame animation (which used to keep CPU pegged at
//! 60 Hz after every completed turn).

use crate::content::SPINNER_FRAMES;
use crate::utils::format_duration;
use protocol::TurnMeta;
use std::time::{Duration, Instant};

/// Phase of the currently-running turn. The spinner animates based on
/// `since` and the phase; `Retrying` additionally carries a delay and
/// attempt counter for the countdown display.
#[derive(Clone, Copy, PartialEq)]
pub enum TurnPhase {
    Working,
    Compacting,
    Retrying { delay: Duration, attempt: u32 },
}

/// Outcome of a completed turn.
#[derive(Clone, Copy, PartialEq)]
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
    /// `pause_started` is set, otherwise just `since.elapsed()` (which
    /// is paused-aware because resume shifts `since` forward).
    fn effective_elapsed(&self) -> Duration {
        match self.pause_started {
            Some(t) => t.duration_since(self.since),
            None => self.since.elapsed(),
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

#[derive(Default)]
pub struct WorkingState {
    live: Option<LiveTurn>,
    last: Option<LastTurn>,
}

impl WorkingState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new live turn, or update the phase of the currently-
    /// running one (keeps `since` and accumulated `tps_samples`).
    pub fn begin(&mut self, phase: TurnPhase) {
        let retry_deadline = match phase {
            TurnPhase::Retrying { delay, .. } => Some(Instant::now() + delay),
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
                    since: Instant::now(),
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
        let (elapsed, avg_tps) = match self.live.take() {
            Some(live) => (live.effective_elapsed(), avg(&live.tps_samples)),
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

    pub fn is_compacting(&self) -> bool {
        matches!(
            self.live.as_ref().map(|l| l.phase),
            Some(TurnPhase::Compacting)
        )
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
            Some(live.effective_elapsed())
        } else {
            self.last.as_ref().map(|l| l.elapsed)
        }
    }

    /// Toggle the paused state on the live turn (if any). While paused,
    /// `effective_elapsed` and the spinner freeze. On resume, `since`
    /// is shifted forward by the pause duration so subsequent reads of
    /// `since.elapsed()` are still correct. Idempotent.
    pub fn set_paused(&mut self, paused: bool) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        match (paused, live.pause_started) {
            (true, None) => live.pause_started = Some(Instant::now()),
            (false, Some(t)) => {
                live.since += t.elapsed();
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
                elapsed_ms: live.effective_elapsed().as_millis() as u64,
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
        Some(SPINNER_FRAMES[crate::content::spinner_frame_index(live.effective_elapsed())])
    }

    /// Headless-safe throbber data.  The caller (statusline composer in
    /// tui) applies theme colours and builds the Lua table.
    pub fn throbber_data(&self, show_tps: bool) -> Vec<ThrobberItem> {
        let mut out = Vec::new();
        if let Some(live) = self.live.as_ref() {
            let elapsed = live.effective_elapsed();
            let idx = crate::content::spinner_frame_index(elapsed);
            match live.phase {
                TurnPhase::Compacting => {
                    out.push(ThrobberItem {
                        text: format!(" {} compacting ", SPINNER_FRAMES[idx]),
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
                }
                TurnPhase::Working | TurnPhase::Retrying { .. } => {
                    let is_retry = matches!(live.phase, TurnPhase::Retrying { .. });
                    out.push(ThrobberItem {
                        text: format!(" {} working ", SPINNER_FRAMES[idx]),
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
                            .map(|t| t.saturating_duration_since(Instant::now()))
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
