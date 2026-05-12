//! Pure-data UI primitives. No terminal deps, no async runtime, no Lua.

pub mod attachment;
pub mod buffer;
pub mod clipboard;
pub mod coords;
pub mod kill_ring;
pub mod text;
pub mod undo;
pub mod wrap;

pub use attachment::ATTACHMENT_MARKER;
pub use clipboard::{Clipboard, NullSink, Osc52Sink, Sink, SystemSink};
pub use smelt_style::{style, theme};
