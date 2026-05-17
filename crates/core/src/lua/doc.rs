//! Doc-collection registry for Lua FFI bindings.
//!
//! [`super::module::LuaMod`] is the entry point — every Lua module
//! constructs one and registers its functions via `.fn_(...)`. The
//! LuaCATS signature is *derived* from the closure's argument-tuple
//! and return types via [`super::lua_type::LuaType`] /
//! [`super::lua_type::LuaTypeTuple`], so it can never drift from the
//! actual mlua marshalling — drift becomes a compile error. The
//! module's tier (Host vs UiHost) surfaces in the generated nav and
//! stub headers so plugin authors can tell at a glance which APIs
//! are headless-safe.
//!
//! `gen-lua-docs` reads [`snapshot`] after spinning up a
//! [`crate::lua::LuaRuntime`] (registration is the side-effect that
//! fills the registry; the closures themselves are never fired) and
//! emits LuaCATS stubs + Markdown reference pages.

use std::sync::Mutex;

use mlua::{FromLuaMulti, IntoLuaMulti, Lua, MaybeSend};

use super::lua_type::{LuaAliasDecl, LuaClassDecl, LuaType, LuaTypeTuple};

/// Which Lua-runtime tier a binding belongs to.
///
/// `Host` bindings (`smelt.fs`, `smelt.http`, `smelt.cell`, …) live in
/// `smelt-core` and work without a terminal UI — headless plugins can
/// call them. `UiHost` bindings (`smelt.win`, `smelt.theme`,
/// `smelt.confirm`, …) live in the TUI crate and crash if invoked
/// without an attached UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Host,
    UiHost,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Host => "Host",
            Tier::UiHost => "UiHost",
        }
    }

    /// One-line description used in the docs index and stub header.
    pub fn description(self) -> &'static str {
        match self {
            Tier::Host => "Available in every runtime, including headless mode.",
            Tier::UiHost => "Requires a terminal UI; calling these from headless mode raises.",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LuaFnMeta {
    pub module: &'static str,
    pub name: &'static str,
    pub doc: &'static str,
    pub sig: String,
    pub tier: Tier,
}

/// Module-level documentation, attached by [`record_module_doc`].
/// Surfaces above the function list in the per-module markdown page
/// and as a top-of-file comment in the LuaCATS stub.
#[derive(Clone, Debug)]
pub struct LuaModuleMeta {
    pub module: &'static str,
    pub doc: &'static str,
}

static REGISTRY: Mutex<Vec<LuaFnMeta>> = Mutex::new(Vec::new());
static CLASSES: Mutex<Vec<LuaClassDecl>> = Mutex::new(Vec::new());
static ALIASES: Mutex<Vec<LuaAliasDecl>> = Mutex::new(Vec::new());
static MODULES: Mutex<Vec<LuaModuleMeta>> = Mutex::new(Vec::new());

pub fn record(meta: LuaFnMeta) {
    if let Ok(mut v) = REGISTRY.lock() {
        v.retain(|m| !(m.module == meta.module && m.name == meta.name));
        v.push(meta);
    }
}

pub fn snapshot() -> Vec<LuaFnMeta> {
    REGISTRY.lock().map(|v| v.clone()).unwrap_or_default()
}

/// Push a class declaration into the side-table. Called from the
/// `LuaType` impl emitted by `#[derive(LuaOpts)]` the first time the
/// type's `lua_type()` runs, so any opts struct that ends up in a
/// `register_fn` sig is automatically discovered.
pub fn record_class(decl: LuaClassDecl) {
    if let Ok(mut v) = CLASSES.lock() {
        v.retain(|c| c.name != decl.name);
        v.push(decl);
    }
}

pub fn classes_snapshot() -> Vec<LuaClassDecl> {
    CLASSES.lock().map(|v| v.clone()).unwrap_or_default()
}

/// Push an alias declaration into the side-table. Called from the
/// `LuaType` impl emitted by `#[derive(LuaAlias)]`.
pub fn record_alias(decl: LuaAliasDecl) {
    if let Ok(mut v) = ALIASES.lock() {
        v.retain(|a| a.name != decl.name);
        v.push(decl);
    }
}

pub fn aliases_snapshot() -> Vec<LuaAliasDecl> {
    ALIASES.lock().map(|v| v.clone()).unwrap_or_default()
}

/// Attach a one-shot description to a Lua module. Call from the
/// module's `register` function before the first `register_fn` so the
/// description shows up in `gen-lua-docs` output. Repeated calls for
/// the same module replace the previous entry.
pub fn record_module_doc(module: &'static str, doc: &'static str) {
    if let Ok(mut v) = MODULES.lock() {
        v.retain(|m| m.module != module);
        v.push(LuaModuleMeta { module, doc });
    }
}

pub fn modules_snapshot() -> Vec<LuaModuleMeta> {
    MODULES.lock().map(|v| v.clone()).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn register_fn_inner<F, A, R>(
    tbl: &mlua::Table,
    module: &'static str,
    name: &'static str,
    doc: &'static str,
    param_names: &[&'static str],
    lua: &Lua,
    f: F,
    tier: Tier,
) -> mlua::Result<()>
where
    F: Fn(&Lua, A) -> mlua::Result<R> + MaybeSend + 'static,
    A: FromLuaMulti + LuaTypeTuple,
    R: IntoLuaMulti + LuaType,
{
    assert_eq!(
        param_names.len(),
        A::ARITY,
        "param_names length ({}) does not match arity ({}) for {}.{}",
        param_names.len(),
        A::ARITY,
        module,
        name
    );
    let sig = format!("fun({}): {}", A::lua_param_list(param_names), R::lua_type());
    tbl.set(name, lua.create_function(f)?)?;
    record(LuaFnMeta {
        module,
        name,
        doc,
        sig,
        tier,
    });
    Ok(())
}
