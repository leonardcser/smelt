//! Doc-collection registry for Lua FFI bindings.
//!
//! [`super::module::LuaMod`] is the entry point - every Lua module
//! constructs one and registers its functions via `.fn_(...)`. The
//! LuaCATS signature is *derived* from the closure's argument-tuple
//! and return types via [`super::lua_type::LuaType`] /
//! [`super::lua_type::LuaTypeTuple`], so it can never drift from the
//! actual mlua marshalling - drift becomes a compile error. The
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
/// `Host` bindings (`smelt.fs`, `smelt.http`, `smelt.signal`, …) live in
/// `smelt-core` and work without a terminal UI - headless plugins can
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Internal,
}

impl Visibility {
    pub fn label(self) -> &'static str {
        match self {
            Visibility::Public => "Public",
            Visibility::Internal => "Internal",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Visibility::Public => "Stable Lua API intended for user config and plugins.",
            Visibility::Internal => "Runtime implementation detail. Bundled Lua may call it, but user config and plugins should not depend on it.",
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
    pub visibility: Visibility,
}

/// Module-level documentation, attached by [`record_module_doc`].
/// Surfaces above the function list in the per-module markdown page
/// and as a top-of-file comment in the LuaCATS stub. `tier` is the
/// declared tier of the module; the page falls back to this when every
/// function is `__`-prefixed and filtered out of the rendered surface.
#[derive(Clone, Debug)]
pub struct LuaModuleMeta {
    pub module: &'static str,
    pub doc: &'static str,
    pub tier: Option<Tier>,
    pub visibility: Visibility,
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
/// `LuaMod::fn_` sig is automatically discovered.
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

/// Attach a one-shot description to a Lua module. Normally fired
/// automatically by [`super::module::LuaMod::under`] / `.sub`; call
/// directly only when extending a table that was already attached by
/// another tier (e.g. `LuaMod::extend`). Repeated calls for the same
/// module replace the previous entry.
pub fn record_module_doc(module: &'static str, doc: &'static str) {
    record_module(module, doc, None);
}

/// Attach a one-shot internal description to a Lua module.
pub fn record_internal_module_doc(module: &'static str, doc: &'static str) {
    record_module_with_visibility(module, doc, None, Visibility::Internal);
}

/// Like [`record_module_doc`] but also pins the module's tier so the
/// rendered page picks up the right label even when every fn is
/// filtered out (private `__`-prefixed names). Called from
/// [`super::module::LuaMod::under`] / `.sub`.
pub fn record_module(module: &'static str, doc: &'static str, tier: Option<Tier>) {
    record_module_with_visibility(module, doc, tier, Visibility::Public);
}

pub fn record_internal_module(module: &'static str, doc: &'static str, tier: Option<Tier>) {
    record_module_with_visibility(module, doc, tier, Visibility::Internal);
}

fn record_module_with_visibility(
    module: &'static str,
    doc: &'static str,
    tier: Option<Tier>,
    visibility: Visibility,
) {
    if let Ok(mut v) = MODULES.lock() {
        v.retain(|m| m.module != module);
        v.push(LuaModuleMeta {
            module,
            doc,
            tier,
            visibility,
        });
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
    visibility: Visibility,
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
        visibility,
    });
    Ok(())
}
