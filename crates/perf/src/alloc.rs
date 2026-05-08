//! Counting global-allocator shim. Process-wide counters always update so peak / current /
//! dealloc stats stay consistent across the whole run; per-thread tallies (used by
//! [`crate::perf::Guard`]) only update when [`enable`] has been called. Install via
//! `#[global_allocator]` in the binary crate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

static ENABLED: AtomicBool = AtomicBool::new(false);

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static DEALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static REALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static BYTES_ALLOCATED: AtomicU64 = AtomicU64::new(0);
static BYTES_DEALLOCATED: AtomicU64 = AtomicU64::new(0);
static CURRENT_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

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
    let mut peak = PEAK_BYTES.load(Relaxed);
    while cur > peak {
        match PEAK_BYTES.compare_exchange_weak(peak, cur, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(p) => peak = p,
        }
    }
}
