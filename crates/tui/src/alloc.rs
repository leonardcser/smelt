//! Re-export the counting allocator from `smelt-core` so the binary
//! crate can install it as `#[global_allocator]`.

pub use smelt_core::alloc::enable;
pub use smelt_core::alloc::Counting;
