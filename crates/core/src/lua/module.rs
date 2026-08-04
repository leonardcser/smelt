//! Path-tracking module builder for the Lua FFI surface.
//!
//! Every `LuaMod` carries its full dotted path (`smelt.http.cache`) and
//! the tier it belongs to. Sub-modules are created with `.sub(name, doc)`
//! and derive their path by concatenation - the path string appears once,
//! at module creation, and the matching `record_module_doc` call fires
//! automatically. Function registration goes through
//! `.fn_(name, doc, params, f)`; the LuaCATS signature is derived from
//! the Rust closure's argument-tuple and return types so drift becomes a
//! compile error.

use mlua::{FromLuaMulti, IntoLuaMulti, Lua, MaybeSend};
use std::sync::{Mutex, OnceLock};

use super::doc::{record_module, register_fn_inner, ApiClassification, Tier};
use super::lua_type::{LuaType, LuaTypeTuple};

const INTERNAL_API_REGISTRY: &str = "__smelt_internal_api";

/// Return the VM-private root that mirrors the public `smelt` namespace for
/// bundled implementation capabilities. The table lives only in the registry;
/// ordinary config and plugin chunks never receive a reference to it.
pub fn internal_api_root(lua: &Lua) -> mlua::Result<mlua::Table> {
    match lua.named_registry_value::<mlua::Value>(INTERNAL_API_REGISTRY)? {
        mlua::Value::Table(root) => Ok(root),
        mlua::Value::Nil => {
            let root = lua.create_table()?;
            lua.set_named_registry_value(INTERNAL_API_REGISTRY, root.clone())?;
            Ok(root)
        }
        other => Err(mlua::Error::external(format!(
            "Internal Lua API registry is {}, expected table",
            other.type_name()
        ))),
    }
}

/// Resolve or create an Internal module table for a canonical `smelt.*` path.
pub fn internal_api_table(lua: &Lua, path: &str) -> mlua::Result<mlua::Table> {
    let mut table = internal_api_root(lua)?;
    let suffix = if path == "smelt" {
        ""
    } else if let Some(suffix) = path.strip_prefix("smelt.") {
        suffix
    } else {
        return Err(mlua::Error::external(format!(
            "Internal Lua API path must be `smelt` or start with `smelt.`: {path}"
        )));
    };
    for part in suffix.split('.').filter(|part| !part.is_empty()) {
        let value = table.raw_get::<mlua::Value>(part)?;
        table = match value {
            mlua::Value::Nil => {
                let child = lua.create_table()?;
                table.raw_set(part, child.clone())?;
                child
            }
            mlua::Value::Table(child) => child,
            other => {
                return Err(mlua::Error::external(format!(
                    "Internal Lua API path component `{part}` is {}, expected table",
                    other.type_name()
                )))
            }
        };
    }
    Ok(table)
}

/// Resolve one callable from the VM-private Internal capability tree.
pub fn internal_api_function(lua: &Lua, module: &str, name: &str) -> mlua::Result<mlua::Function> {
    internal_api_table(lua, module)?.raw_get(name)
}

fn chunk_environment(lua: &Lua) -> mlua::Result<mlua::Table> {
    let env = lua.create_table()?;
    let globals = lua.globals();
    let metatable = lua.create_table()?;
    metatable.raw_set("__index", globals.clone())?;
    metatable.raw_set("__newindex", globals)?;
    env.set_metatable(Some(metatable))?;
    Ok(env)
}

/// A chunk environment for trusted bundled Lua. Public names still resolve
/// through globals, while Internal capabilities are available only through the
/// environment-local `__smelt_internal` binding. Global assignments retain
/// their normal behavior by forwarding through `__newindex`.
pub fn bundled_chunk_environment(lua: &Lua) -> mlua::Result<mlua::Table> {
    let env = chunk_environment(lua)?;
    env.raw_set("__smelt_internal", internal_api_root(lua)?)?;
    Ok(env)
}

/// Bootstrap environment for bundled Lua. Internal capabilities are attached
/// only for sources from the trusted runtime root.
pub fn bootstrap_chunk_environment(lua: &Lua, trusted: bool) -> mlua::Result<mlua::Table> {
    let env = chunk_environment(lua)?;
    if trusted {
        env.raw_set("__smelt_internal", internal_api_root(lua)?)?;
    }
    Ok(env)
}

/// A live Lua module under construction. The `tbl` is already attached
/// to its parent; `path` is the dotted path stored in the doc registry.
pub struct LuaMod<'a> {
    pub tbl: mlua::Table,
    lua: &'a Lua,
    path: &'static str,
    tier: Tier,
    classification: ApiClassification,
}

impl<'a> LuaMod<'a> {
    pub fn supported(
        lua: &'a Lua,
        smelt: &mlua::Table,
        name: &'static str,
        doc: &'static str,
        tier: Tier,
    ) -> mlua::Result<Self> {
        Self::under(lua, smelt, name, doc, tier, ApiClassification::Supported)
    }

    pub fn advanced(
        lua: &'a Lua,
        smelt: &mlua::Table,
        name: &'static str,
        doc: &'static str,
        tier: Tier,
    ) -> mlua::Result<Self> {
        Self::under(lua, smelt, name, doc, tier, ApiClassification::Advanced)
    }

    /// Attach a fresh module named `name` directly under the `smelt` root.
    fn under(
        lua: &'a Lua,
        smelt: &mlua::Table,
        name: &'static str,
        doc: &'static str,
        tier: Tier,
        classification: ApiClassification,
    ) -> mlua::Result<Self> {
        let path: &'static str = intern_path(format!("smelt.{name}"));
        let tbl = lua.create_table()?;
        smelt.set(name, tbl.clone())?;
        record_module(path, doc, Some(tier), classification);
        Ok(Self {
            tbl,
            lua,
            path,
            tier,
            classification,
        })
    }

    /// Wrap a public table so a higher tier can extend its Supported facade.
    pub fn extend_supported(
        lua: &'a Lua,
        tbl: mlua::Table,
        path: &'static str,
        tier: Tier,
    ) -> Self {
        Self {
            tbl,
            lua,
            path,
            tier,
            classification: ApiClassification::Supported,
        }
    }

    /// Take ownership of a public table and declare its Supported facade.
    pub fn own_supported(
        lua: &'a Lua,
        tbl: mlua::Table,
        path: &'static str,
        doc: &'static str,
        tier: Tier,
    ) -> Self {
        record_module(path, doc, Some(tier), ApiClassification::Supported);
        Self {
            tbl,
            lua,
            path,
            tier,
            classification: ApiClassification::Supported,
        }
    }

    /// Add a sub-module under this one. Path becomes `self.path.name`.
    /// Inherits the parent's tier.
    pub fn sub(&self, name: &'static str, doc: &'static str) -> mlua::Result<LuaMod<'a>> {
        self.sub_with_classification(name, doc, self.classification)
    }

    pub fn sub_with_classification(
        &self,
        name: &'static str,
        doc: &'static str,
        classification: ApiClassification,
    ) -> mlua::Result<LuaMod<'a>> {
        let path: &'static str = intern_path(format!("{}.{name}", self.path));
        let tbl = if classification == ApiClassification::Internal {
            internal_api_table(self.lua, path)?
        } else {
            let tbl = self.lua.create_table()?;
            self.tbl.set(name, tbl.clone())?;
            tbl
        };
        record_module(path, doc, Some(self.tier), classification);
        Ok(LuaMod {
            tbl,
            lua: self.lua,
            path,
            tier: self.tier,
            classification,
        })
    }

    /// Register a Lua function at `<self.path>.<name>`. Trait bounds derive the
    /// LuaCATS signature from the Rust types, so signature drift becomes a
    /// compile error.
    pub fn fn_<F, A, R>(
        &self,
        name: &'static str,
        doc: &'static str,
        params: &[&'static str],
        f: F,
    ) -> mlua::Result<()>
    where
        F: Fn(&Lua, A) -> mlua::Result<R> + MaybeSend + 'static,
        A: FromLuaMulti + LuaTypeTuple,
        R: IntoLuaMulti + LuaType,
    {
        register_fn_inner(
            &self.tbl,
            self.path,
            name,
            doc,
            params,
            self.lua,
            f,
            self.tier,
            self.classification,
            false,
        )
    }

    pub fn advanced_fn<F, A, R>(
        &self,
        name: &'static str,
        doc: &'static str,
        params: &[&'static str],
        f: F,
    ) -> mlua::Result<()>
    where
        F: Fn(&Lua, A) -> mlua::Result<R> + MaybeSend + 'static,
        A: FromLuaMulti + LuaTypeTuple,
        R: IntoLuaMulti + LuaType,
    {
        register_fn_inner(
            &self.tbl,
            self.path,
            name,
            doc,
            params,
            self.lua,
            f,
            self.tier,
            ApiClassification::Advanced,
            false,
        )
    }

    /// Register a direct effect that is unavailable while a replacement Lua
    /// generation is being evaluated.
    pub fn live_only_fn<F, A, R>(
        &self,
        name: &'static str,
        doc: &'static str,
        params: &[&'static str],
        f: F,
    ) -> mlua::Result<()>
    where
        F: Fn(&Lua, A) -> mlua::Result<R> + MaybeSend + 'static,
        A: FromLuaMulti + LuaTypeTuple,
        R: IntoLuaMulti + LuaType,
    {
        register_fn_inner(
            &self.tbl,
            self.path,
            name,
            doc,
            params,
            self.lua,
            f,
            self.tier,
            self.classification,
            true,
        )
    }

    /// Register an Internal implementation hook consumed by bundled Lua. The
    /// callable is stored only in the VM-private capability tree.
    pub fn private_fn<F, A, R>(
        &self,
        name: &'static str,
        params: &[&'static str],
        f: F,
    ) -> mlua::Result<()>
    where
        F: Fn(&Lua, A) -> mlua::Result<R> + MaybeSend + 'static,
        A: FromLuaMulti + LuaTypeTuple,
        R: IntoLuaMulti + LuaType,
    {
        register_fn_inner(
            &internal_api_table(self.lua, self.path)?,
            self.path,
            name,
            "Bundled runtime implementation hook.",
            params,
            self.lua,
            f,
            self.tier,
            ApiClassification::Internal,
            false,
        )
    }

    pub fn private_live_only_fn<F, A, R>(
        &self,
        name: &'static str,
        params: &[&'static str],
        f: F,
    ) -> mlua::Result<()>
    where
        F: Fn(&Lua, A) -> mlua::Result<R> + MaybeSend + 'static,
        A: FromLuaMulti + LuaTypeTuple,
        R: IntoLuaMulti + LuaType,
    {
        register_fn_inner(
            &internal_api_table(self.lua, self.path)?,
            self.path,
            name,
            "Bundled runtime implementation hook.",
            params,
            self.lua,
            f,
            self.tier,
            ApiClassification::Internal,
            true,
        )
    }

    /// Install an Internal metamethod on an unexposed metatable. Unlike
    /// namespace capabilities, the callable must remain on `self.tbl` so Lua's
    /// metatable dispatch can reach it.
    pub fn private_metamethod<F, A, R>(
        &self,
        name: &'static str,
        params: &[&'static str],
        f: F,
    ) -> mlua::Result<()>
    where
        F: Fn(&Lua, A) -> mlua::Result<R> + MaybeSend + 'static,
        A: FromLuaMulti + LuaTypeTuple,
        R: IntoLuaMulti + LuaType,
    {
        register_fn_inner(
            &self.tbl,
            self.path,
            name,
            "Internal metatable implementation.",
            params,
            self.lua,
            f,
            self.tier,
            ApiClassification::Internal,
            false,
        )
    }
}

/// Intern a generated module path for the process lifetime. Lua runtimes are
/// recreated during reloads and tests, so retaining one copy per distinct path
/// keeps the documentation registry's static references bounded.
fn intern_path(s: String) -> &'static str {
    static PATHS: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    let mut paths = PATHS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("Lua module path interner poisoned");
    if let Some(path) = paths.iter().copied().find(|path| *path == s) {
        return path;
    }
    let path = Box::leak(s.into_boxed_str());
    paths.push(path);
    path
}
