//! Lightweight allocation + timing instrumentation.
//!
//! Two coordinated pieces:
//! - [`alloc::Counting`] - global allocator shim that bumps process and per-thread counters
//!   when [`alloc::enable`] has been called. Cheap when disabled.
//! - [`perf::begin`] - RAII scope guards that record duration and per-thread alloc deltas under
//!   a `&'static str` label. Snapshot via [`perf::snapshot`], pretty-print via
//!   [`perf::print_summary`].
//!
//! Designed to be embedded in any binary: install `Counting` as `#[global_allocator]`, call
//! `alloc::enable()` and `perf::enable()` early in `main`, sprinkle `let _g = perf::begin(...);`
//! around interesting scopes.

pub mod alloc;
pub mod perf;
