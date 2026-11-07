//! Transcript model and its presentation IR: blocks and tool state, shared
//! content storage, markdown and stream parsing, syntax and diff highlighting,
//! and the builders that turn blocks into styled lines.

pub mod content;
pub mod diff;
pub mod hash;
pub mod paused_timer;
pub mod tool_presentation;
pub mod transcript_content;
pub mod transcript_model;

pub use smelt_buffer::{attachment, buffer, style, theme};
