//! Counting global-allocator shim. Process-wide counters always update so peak / current /
//! dealloc stats stay consistent across the whole run; per-thread tallies (used by
//! [`crate::perf::Guard`]) only update when [`enable`] has been called. Install via
//! `#[global_allocator]` in the binary crate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::spin_loop;
use std::sync::atomic::{
    AtomicBool, AtomicU64, AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};

static ENABLED: AtomicBool = AtomicBool::new(false);

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static DEALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static REALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static BYTES_ALLOCATED: AtomicU64 = AtomicU64::new(0);
static BYTES_DEALLOCATED: AtomicU64 = AtomicU64::new(0);
static CURRENT_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

const PEAK_MEASUREMENT_PHASE_MASK: usize = 0b11;
const PEAK_MEASUREMENT_IDLE: usize = 0;
const PEAK_MEASUREMENT_INITIALIZING: usize = 1;
const PEAK_MEASUREMENT_ACTIVE: usize = 2;
const PEAK_MEASUREMENT_STOPPING: usize = 3;
static PEAK_MEASUREMENT_STATE: AtomicUsize = AtomicUsize::new(PEAK_MEASUREMENT_IDLE);
static PEAK_MEASUREMENT_UPDATERS: AtomicUsize = AtomicUsize::new(0);
static MEASURED_PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static T_ALLOCS: Cell<u64> = const { Cell::new(0) };
    static T_BYTES: Cell<u64> = const { Cell::new(0) };
}

pub fn enable() {
    ENABLED.store(true, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

/// Calling-thread `(alloc_count, alloc_bytes_grown)` totals. Monotonic; take deltas.
/// Used by [`crate::perf::Guard`] to attribute allocs to the thread doing the work.
pub fn thread_snapshot() -> (u64, u64) {
    let a = T_ALLOCS.try_with(|c| c.get()).unwrap_or(0);
    let b = T_BYTES.try_with(|c| c.get()).unwrap_or(0);
    (a, b)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AllocStats {
    pub allocs: u64,
    pub deallocs: u64,
    pub reallocs: u64,
    pub bytes_allocated: u64,
    pub bytes_deallocated: u64,
    pub current_bytes: usize,
    pub peak_bytes: usize,
}

/// Process-wide cumulative allocation stats. Subtract two snapshots for a phase delta.
pub fn snapshot() -> AllocStats {
    AllocStats {
        allocs: ALLOC_COUNT.load(Relaxed),
        deallocs: DEALLOC_COUNT.load(Relaxed),
        reallocs: REALLOC_COUNT.load(Relaxed),
        bytes_allocated: BYTES_ALLOCATED.load(Relaxed),
        bytes_deallocated: BYTES_DEALLOCATED.load(Relaxed),
        current_bytes: CURRENT_BYTES.load(Relaxed),
        peak_bytes: PEAK_BYTES.load(Relaxed),
    }
}

pub fn delta(start: AllocStats, end: AllocStats) -> AllocStats {
    AllocStats {
        allocs: end.allocs.saturating_sub(start.allocs),
        deallocs: end.deallocs.saturating_sub(start.deallocs),
        reallocs: end.reallocs.saturating_sub(start.reallocs),
        bytes_allocated: end.bytes_allocated.saturating_sub(start.bytes_allocated),
        bytes_deallocated: end
            .bytes_deallocated
            .saturating_sub(start.bytes_deallocated),
        current_bytes: end.current_bytes,
        peak_bytes: end.peak_bytes,
    }
}

/// Exclusive process-wide measurement of peak live allocator bytes during one phase.
///
/// The returned peak includes allocations already live when the phase begins. Only one
/// measurement may be active in a process at a time.
pub struct PeakMeasurement {
    start_bytes: usize,
    active_state: usize,
    active: bool,
}

impl PeakMeasurement {
    pub fn start_bytes(&self) -> usize {
        self.start_bytes
    }

    pub fn finish(mut self) -> usize {
        let peak = finish_peak_measurement(self.active_state);
        self.active = false;
        peak
    }
}

impl Drop for PeakMeasurement {
    fn drop(&mut self) {
        if self.active {
            let _ = finish_peak_measurement(self.active_state);
        }
    }
}

pub fn begin_peak_measurement() -> PeakMeasurement {
    let idle_state = PEAK_MEASUREMENT_STATE.load(Acquire);
    if idle_state & PEAK_MEASUREMENT_PHASE_MASK != PEAK_MEASUREMENT_IDLE {
        panic!("an allocator peak measurement is already active");
    }
    let initializing_state = idle_state
        .checked_add(PEAK_MEASUREMENT_INITIALIZING)
        .expect("allocator peak measurement generation overflowed");
    PEAK_MEASUREMENT_STATE
        .compare_exchange(idle_state, initializing_state, AcqRel, Acquire)
        .unwrap_or_else(|_| panic!("an allocator peak measurement is already active"));

    let start_bytes = CURRENT_BYTES.load(Acquire);
    MEASURED_PEAK_BYTES.store(start_bytes, Relaxed);
    let active_state = idle_state
        .checked_add(PEAK_MEASUREMENT_ACTIVE)
        .expect("allocator peak measurement generation overflowed");
    PEAK_MEASUREMENT_STATE.store(active_state, Release);
    update_measured_peak(CURRENT_BYTES.load(Acquire));
    PeakMeasurement {
        start_bytes,
        active_state,
        active: true,
    }
}

fn finish_peak_measurement(active_state: usize) -> usize {
    let stopping_state = active_state
        .checked_add(PEAK_MEASUREMENT_STOPPING - PEAK_MEASUREMENT_ACTIVE)
        .expect("allocator peak measurement generation overflowed");
    PEAK_MEASUREMENT_STATE
        .compare_exchange(active_state, stopping_state, AcqRel, Acquire)
        .expect("allocator peak measurement is not active");
    while PEAK_MEASUREMENT_UPDATERS.load(Acquire) != 0 {
        spin_loop();
    }
    let peak = MEASURED_PEAK_BYTES.load(Acquire);
    let idle_state = stopping_state
        .checked_add(1)
        .expect("allocator peak measurement generation overflowed");
    PEAK_MEASUREMENT_STATE.store(idle_state, Release);
    peak
}

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let size = layout.size();
            ALLOC_COUNT.fetch_add(1, Relaxed);
            BYTES_ALLOCATED.fetch_add(size as u64, Relaxed);
            let cur = CURRENT_BYTES.fetch_add(size, Relaxed) + size;
            update_peak(cur);
            if ENABLED.load(Relaxed) {
                // `try_with` because the allocator can run during TLS teardown.
                let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
                let _ = T_BYTES.try_with(|c| c.set(c.get() + size as u64));
            }
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        DEALLOC_COUNT.fetch_add(1, Relaxed);
        BYTES_DEALLOCATED.fetch_add(layout.size() as u64, Relaxed);
        CURRENT_BYTES.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            let size = layout.size();
            ALLOC_COUNT.fetch_add(1, Relaxed);
            BYTES_ALLOCATED.fetch_add(size as u64, Relaxed);
            let cur = CURRENT_BYTES.fetch_add(size, Relaxed) + size;
            update_peak(cur);
            if ENABLED.load(Relaxed) {
                let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
                let _ = T_BYTES.try_with(|c| c.set(c.get() + size as u64));
            }
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            REALLOC_COUNT.fetch_add(1, Relaxed);
            let old = layout.size();
            if new_size >= old {
                let grown = new_size - old;
                BYTES_ALLOCATED.fetch_add(grown as u64, Relaxed);
                let cur = CURRENT_BYTES.fetch_add(grown, Relaxed) + grown;
                update_peak(cur);
                if ENABLED.load(Relaxed) {
                    let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
                    let _ = T_BYTES.try_with(|c| c.set(c.get() + grown as u64));
                }
            } else {
                let shrunk = old - new_size;
                BYTES_DEALLOCATED.fetch_add(shrunk as u64, Relaxed);
                CURRENT_BYTES.fetch_sub(shrunk, Relaxed);
                if ENABLED.load(Relaxed) {
                    let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
                }
            }
        }
        p
    }
}

fn update_peak(cur: usize) {
    update_atomic_peak(&PEAK_BYTES, cur);
    update_measured_peak(cur);
}

fn update_measured_peak(cur: usize) {
    let state = PEAK_MEASUREMENT_STATE.load(Acquire);
    if state & PEAK_MEASUREMENT_PHASE_MASK != PEAK_MEASUREMENT_ACTIVE {
        return;
    }

    PEAK_MEASUREMENT_UPDATERS.fetch_add(1, AcqRel);
    if PEAK_MEASUREMENT_STATE.load(Acquire) == state {
        update_atomic_peak(&MEASURED_PEAK_BYTES, cur);
    }
    PEAK_MEASUREMENT_UPDATERS.fetch_sub(1, Release);
}

fn update_atomic_peak(target: &AtomicUsize, cur: usize) {
    let mut peak = target.load(Relaxed);
    while cur > peak {
        match target.compare_exchange_weak(peak, cur, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(observed) => peak = observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_peak_measurement_includes_baseline_and_concurrent_updates() {
        let original_current = CURRENT_BYTES.swap(4_096, AcqRel);

        let measurement = begin_peak_measurement();
        assert_eq!(measurement.start_bytes(), 4_096);
        std::thread::scope(|scope| {
            for peak in [8_192, 12_288, 16_384] {
                scope.spawn(move || update_measured_peak(peak));
            }
        });
        assert_eq!(measurement.finish(), 16_384);

        let measurement = begin_peak_measurement();
        let nested = std::panic::catch_unwind(begin_peak_measurement);
        assert!(nested.is_err());
        drop(measurement);

        let measurement = begin_peak_measurement();
        assert_eq!(measurement.start_bytes(), 4_096);
        assert_eq!(measurement.finish(), 4_096);

        CURRENT_BYTES.store(original_current, Release);
    }
}
