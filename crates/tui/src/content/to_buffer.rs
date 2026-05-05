//! Buffer rendering glue.
//!
//! `SpanCollector` (in `smelt_core::content::layout_out`) writes
//! directly into a `Buffer` with theme-resolved styles, so the old
//! projection layer (`render_into_buffer` / `project_display_line` /
//! `apply_to_buffer` / `buffer_into_collector`) is gone. This file
//! re-exports the headless-safe entry points so callers don't have to
//! reach into `smelt_core::content::layout_out` for the common case.

pub use smelt_core::content::layout_out::{
    render_into as render_into_buffer, replay_buffer_into, replay_buffer_row_into,
};
