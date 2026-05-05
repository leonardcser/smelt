//! Buffer — re-exports from `smelt_core::buffer`.
//!
//! `Buffer` is pure content data (lines + namespaced extmarks) and
//! lives in `core` so headless mode reads transcript content through
//! the same surface as the TUI. This module forwards every public
//! type so existing call sites don't have to chase the rename.

pub use smelt_core::buffer::*;
