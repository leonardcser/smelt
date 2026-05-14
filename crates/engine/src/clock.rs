//! Process-wide source of time, abstracted so deterministic simulation tests
//! and fuzz scenarios can replace real clocks with manually-advanced ones.
//!
//! Two narrow traits: [`MonoClock`] for [`Instant`], [`WallClock`] for
//! [`SystemTime`]. The combined [`Clock`] alias is for the rare site that
//! genuinely needs both (logging, session timestamps + monotonic gating).
//!
//! Methods are named after the type they return (`instant_now` /
//! `system_now`) so call sites holding `&dyn Clock` don't need UFCS to
//! disambiguate.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Source of monotonic time (`Instant`).
pub trait MonoClock: Send + Sync {
    fn instant_now(&self) -> Instant;
}

/// Source of wall-clock time (`SystemTime`).
pub trait WallClock: Send + Sync {
    fn system_now(&self) -> SystemTime;
}

/// Convenience alias for code that needs both flavors of time. Implemented
/// automatically for any type satisfying both narrower traits.
pub trait Clock: MonoClock + WallClock {}
impl<T: MonoClock + WallClock + ?Sized> Clock for T {}

/// Production clock: delegates to `Instant::now` / `SystemTime::now`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealClock;

impl MonoClock for RealClock {
    fn instant_now(&self) -> Instant {
        Instant::now()
    }
}

impl WallClock for RealClock {
    fn system_now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Manually-advanced clock for deterministic simulation and fuzz scenarios.
///
/// Holds an `Instant` + `SystemTime`; [`advance`](Self::advance) bumps both
/// by the same duration so monotonic and wall views stay coherent. Reads
/// take `&self`, so the clock can be shared as `&dyn MonoClock` /
/// `&dyn WallClock` / `&dyn Clock`.
pub struct VirtualClock {
    mono: Mutex<Instant>,
    wall: Mutex<SystemTime>,
}

impl VirtualClock {
    /// `Instant` has no constructor for an arbitrary value, so callers
    /// typically pass `Instant::now()` once at scenario start and let
    /// [`advance`](Self::advance) drive subsequent reads.
    pub fn new(start_mono: Instant, start_wall: SystemTime) -> Self {
        Self {
            mono: Mutex::new(start_mono),
            wall: Mutex::new(start_wall),
        }
    }

    /// Advance both monotonic and wall views by the same duration.
    pub fn advance(&self, dur: Duration) {
        *self.mono.lock().unwrap() += dur;
        *self.wall.lock().unwrap() += dur;
    }
}

impl MonoClock for VirtualClock {
    fn instant_now(&self) -> Instant {
        *self.mono.lock().unwrap()
    }
}

impl WallClock for VirtualClock {
    fn system_now(&self) -> SystemTime {
        *self.wall.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_clock_instant_now_is_monotonically_non_decreasing() {
        let c = RealClock;
        let a = c.instant_now();
        let b = c.instant_now();
        assert!(b >= a);
    }

    #[test]
    fn real_clock_system_now_is_close_to_std_system_now() {
        let c = RealClock;
        let before = SystemTime::now();
        let from_clock = c.system_now();
        let after = SystemTime::now();
        assert!(from_clock >= before);
        assert!(from_clock <= after);
    }

    #[test]
    fn virtual_clock_advance_bumps_mono_and_wall_by_the_same_duration() {
        let start_mono = Instant::now();
        let start_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let c = VirtualClock::new(start_mono, start_wall);

        assert_eq!(c.instant_now(), start_mono);
        assert_eq!(c.system_now(), start_wall);

        c.advance(Duration::from_millis(250));
        assert_eq!(c.instant_now(), start_mono + Duration::from_millis(250));
        assert_eq!(c.system_now(), start_wall + Duration::from_millis(250));

        c.advance(Duration::from_secs(5));
        assert_eq!(c.instant_now(), start_mono + Duration::from_millis(5250));
        assert_eq!(c.system_now(), start_wall + Duration::from_millis(5250));
    }

    #[test]
    fn virtual_clock_reads_do_not_advance_on_their_own() {
        let c = VirtualClock::new(Instant::now(), SystemTime::now());
        let mono = c.instant_now();
        let wall = c.system_now();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(c.instant_now(), mono);
        assert_eq!(c.system_now(), wall);
    }

    #[test]
    fn clock_alias_is_satisfied_by_both_impls() {
        fn takes_clock(_: &dyn Clock) {}
        takes_clock(&RealClock);
        takes_clock(&VirtualClock::new(Instant::now(), SystemTime::now()));
    }

    #[test]
    fn virtual_clock_advance_is_visible_through_dyn_clock() {
        let start_mono = Instant::now();
        let start_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let vc = VirtualClock::new(start_mono, start_wall);
        let dyn_clock: &dyn Clock = &vc;
        assert_eq!(MonoClock::instant_now(dyn_clock), start_mono);
        vc.advance(Duration::from_millis(100));
        assert_eq!(
            MonoClock::instant_now(dyn_clock),
            start_mono + Duration::from_millis(100)
        );
        assert_eq!(
            WallClock::system_now(dyn_clock),
            start_wall + Duration::from_millis(100)
        );
    }
}
