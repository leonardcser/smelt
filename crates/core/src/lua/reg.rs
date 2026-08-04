//! `smelt.Reg` - uniform registration handle.
//!
//! Every reactive-subscription API (`win:key`, `win:on`, `timer.set`,
//! `timer.every`, `cell:subscribe`, `cell.glob`) returns a `Reg`. The
//! handle carries a type-erased undoer that fires exactly once on
//! `:remove()`; subsequent calls return `false`.

use crate::lua::lua_type::LuaType;
use mlua::prelude::*;
use std::cell::RefCell;

/// Boxed `FnOnce() -> bool` undoer, parked in a `RefCell` so `:remove()`
/// can take it on first call.
type Undoer = Box<dyn FnOnce() -> bool>;

/// Userdata wrapping a one-shot undoer. Constructed by Rust callers via
/// [`LuaReg::new`] and yielded to Lua as `smelt.Reg`.
pub struct LuaReg {
    undoer: RefCell<Option<Undoer>>,
}

impl LuaReg {
    pub fn new<F: FnOnce() -> bool + 'static>(f: F) -> Self {
        Self {
            undoer: RefCell::new(Some(Box::new(f))),
        }
    }
}

impl LuaType for LuaReg {
    fn lua_type() -> String {
        "smelt.Reg".into()
    }
}

impl mlua::UserData for LuaReg {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, _, ()| Ok("Reg".to_string()));
        methods.add_method("remove", |_, this, ()| -> LuaResult<bool> {
            let undoer = this.undoer.borrow_mut().take();
            Ok(match undoer {
                Some(f) => f(),
                None => false,
            })
        });
    }
}

/// Register the `smelt.Reg` class doc once at startup. Called from the
/// host-tier API registration so generated LuaCATS picks up the type.
pub fn register_class_doc() {
    use crate::lua::doc::record_class;
    use crate::lua::lua_type::LuaClassDecl;
    record_class(LuaClassDecl {
        name: "smelt.Reg",
        doc: "Registration handle returned by every reactive-subscription API. \
`:remove()` undoes the binding (frees the underlying callback / cancels the timer / drops the subscription). \
Idempotent: subsequent calls return `false`.",
        classification: crate::lua::doc::ApiClassification::Supported,
        fields: crate::class_methods! {
            "remove" => fn() -> bool, "Undo the registration. Returns `true` the first time; `false` on subsequent calls or when the underlying target is already gone.",
        },
    });
}
