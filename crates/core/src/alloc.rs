//! Counting global-allocator shim. When enabled, bumps per-thread atomic counters on every alloc
//! so `perf::Guard` can attribute allocation deltas to labelled spans. Overhead when disabled is
//! one relaxed atomic load per alloc. Install via `#[global_allocator]` in the binary crate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static T_ALLOCS: Cell<u64> = const { Cell::new(0) };
    static T_BYTES: Cell<u64> = const { Cell::new(0) };
}

pub fn enable() {
    ENABLED.store(true, Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Calling-thread `(alloc_count, alloc_bytes)` totals. Monotonic; take deltas.
pub(crate) fn snapshot() -> (u64, u64) {
    let a = T_ALLOCS.try_with(|c| c.get()).unwrap_or(0);
    let b = T_BYTES.try_with(|c| c.get()).unwrap_or(0);
    (a, b)
}

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            ALLOC_COUNT.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
            // `try_with` because the allocator can run during TLS teardown.
            let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
            let _ = T_BYTES.try_with(|c| c.set(c.get() + layout.size() as u64));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            ALLOC_COUNT.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
            let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
            let _ = T_BYTES.try_with(|c| c.set(c.get() + layout.size() as u64));
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            ALLOC_COUNT.fetch_add(1, Relaxed);
            // Count the growth only; shrinks contribute zero bytes.
            if new_size > layout.size() {
                let grown = (new_size - layout.size()) as u64;
                ALLOC_BYTES.fetch_add(grown, Relaxed);
                let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
                let _ = T_BYTES.try_with(|c| c.set(c.get() + grown));
            } else {
                let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
            }
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
