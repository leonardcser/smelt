//! Process allocator for every smelt binary: mimalloc behind the counting
//! shim. Install with
//! `#[global_allocator] static ALLOCATOR: Allocator = ALLOCATOR;` and call
//! [`configure`] first thing in `main`.

use libmimalloc_sys::{mi_option_set, mi_option_t};

pub type Allocator = smelt_perf::alloc::Counting<mimalloc::MiMalloc>;
pub const ALLOCATOR: Allocator = smelt_perf::alloc::Counting::new(mimalloc::MiMalloc);

/// `mi_option_purge_delay` in mimalloc v3's `mimalloc.h`; the bindings do not
/// export it.
const MI_OPTION_PURGE_DELAY: mi_option_t = 15;

/// Milliseconds before mimalloc returns freed pages to the OS. Its 1 s default
/// keeps bursts resident long after an interactive session goes idle.
const PURGE_DELAY_MS: i64 = 100;

/// Tunes the allocator for a long-lived interactive process. mimalloc maps 1
/// GiB-aligned arenas, which transparent huge pages back with 2 MiB pages and
/// inflate resident memory by tens of MiB for little speedup.
pub fn configure() {
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_THP_DISABLE only sets a flag on the calling process.
    unsafe {
        libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0);
    }
    // SAFETY: mimalloc options are atomics that may change at any time.
    unsafe { mi_option_set(MI_OPTION_PURGE_DELAY, PURGE_DELAY_MS as _) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_delay_index_matches_linked_mimalloc() {
        // SAFETY: reading an option has no preconditions.
        let default = unsafe { libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY) };
        assert_eq!(default, 1000, "mimalloc moved mi_option_purge_delay");
    }
}
