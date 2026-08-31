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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use mlua::{FromLuaMulti, IntoLuaMulti, Lua, MaybeSend};

use super::lua_type::{LuaAliasDecl, LuaClassDecl, LuaType, LuaTypeTuple};

/// Which Lua-runtime tier a binding belongs to.
///
/// `Host` bindings (`smelt.fs`, `smelt.http`, `smelt.signal`, …) live in
/// `smelt-core` and work without a terminal UI - headless plugins can
/// call them. `UiHost` bindings (`smelt.win`, `smelt.theme`,
/// `smelt.confirm`, …) live in the TUI crate and return a Lua error when
/// invoked without an active terminal UI entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Host,
    UiHost,
}

#[derive(Clone, Copy)]
struct UiHostAvailability(fn() -> bool);

struct UiHostDeclarationContext(AtomicUsize);

/// Install the frontend-owned availability check applied to every UiHost binding.
pub fn install_ui_host_availability(lua: &Lua, available: fn() -> bool) {
    lua.set_app_data(UiHostAvailability(available));
}

/// Allow bundled declarations to construct inert UiHost handles without
/// claiming that a live terminal is available. Runtime calls remain gated by
/// the frontend-owned availability check.
pub fn with_ui_host_declarations<R>(lua: &Lua, body: impl FnOnce() -> R) -> R {
    if lua.app_data_ref::<UiHostDeclarationContext>().is_none() {
        lua.set_app_data(UiHostDeclarationContext(AtomicUsize::new(0)));
    }
    lua.app_data_ref::<UiHostDeclarationContext>()
        .expect("declaration context installed")
        .0
        .fetch_add(1, Ordering::AcqRel);

    struct Restore<'lua>(&'lua Lua);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            if let Some(context) = self.0.app_data_ref::<UiHostDeclarationContext>() {
                context.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    let _restore = Restore(lua);
    body()
}

fn ui_host_available(lua: &Lua) -> bool {
    lua.app_data_ref::<UiHostAvailability>()
        .is_some_and(|available| (available.0)())
}

fn ui_host_declarations_active(lua: &Lua) -> bool {
    lua.app_data_ref::<UiHostDeclarationContext>()
        .is_some_and(|context| context.0.load(Ordering::Acquire) > 0)
}

pub(crate) fn require_ui_host(lua: &Lua, api: &str) -> mlua::Result<()> {
    if ui_host_available(lua) || ui_host_declarations_active(lua) {
        Ok(())
    } else {
        Err(mlua::Error::RuntimeError(format!(
            "{api} requires an active terminal UI"
        )))
    }
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Host => "Host",
            Tier::UiHost => "UiHost",
        }
    }

    pub fn from_annotation(value: &str) -> Option<Self> {
        match value {
            "host" => Some(Self::Host),
            "ui_host" => Some(Self::UiHost),
            _ => None,
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

/// Audience and stability classification for one Lua API surface.
///
/// Supported APIs are the primary plugin facade. Advanced APIs deliberately
/// expose lower-level composition primitives; both remain user-callable,
/// documented, and typed while the alpha API evolves. Internal APIs are
/// implementation capabilities supplied only to bundled runtime code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ApiClassification {
    Supported,
    Advanced,
    Internal,
}

impl ApiClassification {
    pub fn label(self) -> &'static str {
        match self {
            Self::Supported => "Supported",
            Self::Advanced => "Advanced",
            Self::Internal => "Internal",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Supported => "Primary alpha facade for user config and plugins.",
            Self::Advanced => "Documented low-level capability for plugins that need full control. It may evolve more freely than the Supported facade.",
            Self::Internal => "Bundled runtime implementation capability. It is not part of the user-facing API and is excluded from documentation and completion.",
        }
    }

    pub fn is_user_facing(self) -> bool {
        self != Self::Internal
    }
}

struct CandidateGate(AtomicBool);

pub(crate) fn begin_candidate_load(lua: &Lua) {
    lua.set_app_data(CandidateGate(AtomicBool::new(false)));
}

pub(crate) fn commit_candidate_load(lua: &Lua) -> mlua::Result<()> {
    let gate = lua
        .app_data_ref::<CandidateGate>()
        .ok_or_else(|| mlua::Error::external("Lua candidate gate is unavailable"))?;
    gate.0.store(true, Ordering::Release);
    Ok(())
}

pub(crate) fn candidate_is_loading(lua: &Lua) -> bool {
    lua.app_data_ref::<CandidateGate>()
        .is_some_and(|gate| !gate.0.load(Ordering::Acquire))
}

pub(crate) fn require_live(lua: &Lua, api: &str) -> mlua::Result<()> {
    if candidate_is_loading(lua) {
        Err(mlua::Error::RuntimeError(format!(
            "{api} is unavailable while loading a Lua candidate"
        )))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct LuaFnMeta {
    pub module: &'static str,
    pub name: &'static str,
    pub doc: &'static str,
    pub sig: String,
    pub tier: Tier,
    pub classification: ApiClassification,
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
    pub classification: ApiClassification,
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

/// Attach a one-shot description and explicit classification to a Lua module.
/// Normally fired automatically by [`super::module::LuaMod::under`] / `.sub`;
/// call directly only for bundled-Lua namespaces that do not have a Rust module
/// builder. Repeated calls for the same module replace the previous entry.
pub fn record_module_doc(
    module: &'static str,
    doc: &'static str,
    classification: ApiClassification,
) {
    record_module(module, doc, None, classification);
}

/// Like [`record_module_doc`] but also pins the module's tier so the rendered
/// page has a label even when it contains no user-facing functions.
pub fn record_module(
    module: &'static str,
    doc: &'static str,
    tier: Option<Tier>,
    classification: ApiClassification,
) {
    if let Ok(mut v) = MODULES.lock() {
        v.retain(|m| m.module != module);
        v.push(LuaModuleMeta {
            module,
            doc,
            tier,
            classification,
        });
    }
}

pub fn modules_snapshot() -> Vec<LuaModuleMeta> {
    MODULES.lock().map(|v| v.clone()).unwrap_or_default()
}

/// Resolve a named class or alias to the most specific declared namespace.
/// Derived type declarations use this so their classification cannot drift from
/// the module that owns them.
pub fn classification_for_type(name: &str) -> ApiClassification {
    MODULES
        .lock()
        .expect("Lua module registry poisoned")
        .iter()
        .filter(|module| {
            name == module.module
                || name
                    .strip_prefix(module.module)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        })
        .max_by_key(|module| module.module.len())
        .unwrap_or_else(|| panic!("Lua type `{name}` has no classified owning module"))
        .classification
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
    classification: ApiClassification,
    live_only: bool,
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
    let api = format!("{module}.{name}");
    let function = lua.create_function(move |lua, args| {
        if live_only {
            require_live(lua, &api)?;
        }
        if tier == Tier::UiHost {
            require_ui_host(lua, &api)?;
        }
        f(lua, args)
    })?;
    tbl.set(name, function)?;
    record(LuaFnMeta {
        module,
        name,
        doc,
        sig,
        tier,
        classification,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::module::LuaMod;

    #[test]
    fn live_only_functions_are_blocked_until_candidate_commit() {
        let lua = Lua::new();
        let module = LuaMod::own_supported(
            &lua,
            lua.create_table().unwrap(),
            "smelt.effect_test",
            "Effect guard test module.",
            Tier::Host,
        );
        module
            .live_only_fn("effect", "Direct effect.", &[], |_, ()| Ok(()))
            .unwrap();

        begin_candidate_load(&lua);
        let effect: mlua::Function = module.tbl.get("effect").unwrap();
        assert!(effect
            .call::<()>(())
            .unwrap_err()
            .to_string()
            .contains("smelt.effect_test.effect is unavailable while loading a Lua candidate"));
        commit_candidate_load(&lua).unwrap();
        effect.call::<()>(()).unwrap();
    }

    #[test]
    fn ui_host_and_private_storage_are_enforced() {
        let lua = Lua::new();
        let module = LuaMod::own_supported(
            &lua,
            lua.create_table().unwrap(),
            "smelt.ui_test",
            "UI test module.",
            Tier::UiHost,
        );
        module
            .fn_("visible", "Visible call.", &[], |_, ()| Ok("visible"))
            .unwrap();
        module
            .private_fn("hidden", &[], |_, ()| Ok("hidden"))
            .unwrap();
        module
            .private_metamethod("__call", &[], |_, ()| Ok("metamethod"))
            .unwrap();

        let visible: mlua::Function = module.tbl.get("visible").unwrap();
        assert!(visible.call::<String>(()).is_err());
        assert_eq!(
            with_ui_host_declarations(&lua, || visible.call::<String>(()).unwrap()),
            "visible"
        );
        assert!(visible.call::<String>(()).is_err());
        assert!(matches!(
            module.tbl.raw_get::<mlua::Value>("hidden").unwrap(),
            mlua::Value::Nil
        ));
        let hidden =
            crate::lua::module::internal_api_function(&lua, "smelt.ui_test", "hidden").unwrap();
        assert!(
            crate::lua::module::internal_api_function(&lua, "smelt.missing", "hidden")
                .unwrap_err()
                .to_string()
                .contains("Internal Lua API module is not registered: smelt.missing")
        );
        let internal = crate::lua::module::internal_api_root(&lua).unwrap();
        assert!(matches!(
            internal.raw_get::<mlua::Value>("missing").unwrap(),
            mlua::Value::Nil
        ));
        install_ui_host_availability(&lua, || true);
        assert_eq!(hidden.call::<String>(()).unwrap(), "hidden");
        assert!(matches!(
            module.tbl.raw_get::<mlua::Value>("__call").unwrap(),
            mlua::Value::Function(_)
        ));
    }
}
