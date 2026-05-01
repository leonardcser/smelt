//! Rust-authored dialogs. `confirm` is the security-critical
//! permission + diff-preview dialog; the Lua-driven dialogs open from
//! `crate::lua::ui_ops`.

pub(crate) mod confirm;
pub(crate) mod confirm_preview;
