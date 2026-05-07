//! Pure-data UI primitives shared between headless runtimes and
//! terminal frontends. No terminal deps, no async runtime, no Lua.
//!
//! - `buffer` — Buffer + extmarks + namespaces + parser trait.
//! - `style` / `theme` — color + style data + named highlight groups.
//! - `clipboard` / `kill_ring` — text yank/paste + emacs-style ring.
//! - `undo` — per-buffer undo/redo history.
//! - `attachment` — image/paste attachment store.
//! - `wrap` — display-column word wrap.

pub mod attachment;
pub mod buffer;
pub mod clipboard;
pub mod kill_ring;
pub mod style;
pub mod theme;
pub mod undo;
pub mod wrap;

pub use clipboard::{Clipboard, NullSink, Osc52Sink, Sink, SystemSink};
