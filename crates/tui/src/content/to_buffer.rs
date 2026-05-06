//! Buffer rendering glue.
//!
//! `LineBuilder` (in `smelt_core::content::builder`) writes
//! directly into a `Buffer` with theme-resolved styles, so the old
//! projection layer (`render_into_buffer` / `project_display_line` /
//! `apply_to_buffer` / `buffer_into_collector`) is gone. This file
//! re-exports the headless-safe entry points so callers don't have to
//! reach into `smelt_core::content::builder` for the common case.

pub use smelt_core::content::builder::render_into as render_into_buffer;
