//! Story groups. Each `pub mod` is one file in `stories/<name>.rs`
//! containing a cluster of `story!` invocations. Adding a new group
//! is one line here + one new file.

//! Chrome + layout stories live in `crates/term/tests/storybook/`
//! since they exercise the pure-renderer surface; this directory now
//! covers only stories that depend on smelt-edit (Window, Buffer,
//! Vim, Overlay) or smelt-buffer (Theme/HlGroup integration).

pub mod buffer;
pub mod overlays;
pub mod theme;
pub mod vim;
