//! Re-exports the counting allocator from `smelt-core` for use as `#[global_allocator]`.

pub use smelt_core::alloc::enable;
pub use smelt_core::alloc::Counting;
