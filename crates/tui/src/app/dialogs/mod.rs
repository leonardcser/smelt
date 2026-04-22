//! Per-dialog open/close logic for builtin floats. `confirm` stays in
//! Rust (security gate + diff preview). `lua_dialog` / `lua_picker`
//! are the dispatchers that realise dialogs described by plugin Lua
//! tasks. State shared across callbacks lives in `Rc<RefCell<_>>`,
//! captured by the closures.

pub mod confirm;
pub mod lua_dialog;
pub mod lua_picker;
