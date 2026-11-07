//! Pure-data UI primitives. No terminal deps, no async runtime, no Lua.

pub mod attached;
pub mod attachment;
pub mod buffer;
pub mod clipboard;
pub mod coords;
pub mod document;
pub mod inline_line;
pub mod kill_ring;
pub mod undo;
pub mod wrap;
pub mod wrap_layout;

pub use clipboard::{Clipboard, NullSink, Osc52Sink, Sink, SystemSink};
pub use smelt_style::{cell_width, palette, style, theme};
pub use smelt_text as text;
pub use smelt_text::ATTACHMENT_MARKER;
