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

use super::doc::{record_internal_module, record_module, register_fn_inner, Tier, Visibility};
use super::lua_type::{LuaType, LuaTypeTuple};

/// A live Lua module under construction. The `tbl` is already attached
/// to its parent; `path` is the dotted path stored in the doc registry.
pub struct LuaMod<'a> {
    pub tbl: mlua::Table,
    lua: &'a Lua,
    path: &'static str,
    tier: Tier,
}

impl<'a> LuaMod<'a> {
    /// Attach a fresh module named `name` directly under the `smelt` root,
    /// recording its module-level doc. Use this in every per-namespace
    /// `register` function.
    pub fn under(
        lua: &'a Lua,
        smelt: &mlua::Table,
        name: &'static str,
        doc: &'static str,
        tier: Tier,
    ) -> mlua::Result<Self> {
        let path: &'static str = leak(format!("smelt.{name}"));
        let tbl = lua.create_table()?;
        record_module(path, doc, Some(tier));
        smelt.set(name, tbl.clone())?;
        Ok(Self {
            tbl,
            lua,
            path,
            tier,
        })
    }

    pub fn under_internal(
        lua: &'a Lua,
        smelt: &mlua::Table,
        name: &'static str,
        doc: &'static str,
        tier: Tier,
    ) -> mlua::Result<Self> {
        let path: &'static str = leak(format!("smelt.{name}"));
        let tbl = lua.create_table()?;
        record_internal_module(path, doc, Some(tier));
        smelt.set(name, tbl.clone())?;
        Ok(Self {
            tbl,
            lua,
            path,
            tier,
        })
    }

    /// Wrap an already-attached table at `path` so a higher tier can
    /// extend it (e.g. the TUI adding UiHost fns to host-tier
    /// `smelt.cmd`). Does not record the module doc - the original owner
    /// already did.
    pub fn extend(lua: &'a Lua, tbl: mlua::Table, path: &'static str, tier: Tier) -> Self {
        Self {
            tbl,
            lua,
            path,
            tier,
        }
    }

    /// Take ownership of a `tbl` that the caller already attached (e.g.
    /// the root `smelt` table, or `smelt_keymap` passed in from the
    /// dispatcher). Records the module doc + tier as if `under` had
    /// created the table.
    pub fn own(
        lua: &'a Lua,
        tbl: mlua::Table,
        path: &'static str,
        doc: &'static str,
        tier: Tier,
    ) -> Self {
        record_module(path, doc, Some(tier));
        Self {
            tbl,
            lua,
            path,
            tier,
        }
    }

    /// Add a sub-module under this one. Path becomes `self.path.name`.
    /// Inherits the parent's tier.
    pub fn sub(&self, name: &'static str, doc: &'static str) -> mlua::Result<LuaMod<'a>> {
        let path: &'static str = leak(format!("{}.{name}", self.path));
        let tbl = self.lua.create_table()?;
        record_module(path, doc, Some(self.tier));
        self.tbl.set(name, tbl.clone())?;
        Ok(LuaMod {
            tbl,
            lua: self.lua,
            path,
            tier: self.tier,
        })
    }

    pub fn sub_internal(&self, name: &'static str, doc: &'static str) -> mlua::Result<LuaMod<'a>> {
        let path: &'static str = leak(format!("{}.{name}", self.path));
        let tbl = self.lua.create_table()?;
        record_internal_module(path, doc, Some(self.tier));
        self.tbl.set(name, tbl.clone())?;
        Ok(LuaMod {
            tbl,
            lua: self.lua,
            path,
            tier: self.tier,
        })
    }

    /// Register a Lua function at `<self.path>.<name>` with the given
    /// doc and param names. Trait bounds derive the LuaCATS signature
    /// from the Rust types - drift becomes a compile error.
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
            Visibility::Public,
        )
    }

    pub fn internal_fn<F, A, R>(
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
            Visibility::Internal,
        )
    }

    /// Register a private Lua function at `<self.path>.<name>` without
    /// adding it to the generated API docs or LuaCATS stubs. Use this for
    /// Rust-backed implementation hooks consumed by bundled Lua wrappers.
    pub fn private_fn<F, A, R>(
        &self,
        name: &'static str,
        params: &[&'static str],
        f: F,
    ) -> mlua::Result<()>
    where
        F: Fn(&Lua, A) -> mlua::Result<R> + MaybeSend + 'static,
        A: FromLuaMulti + LuaTypeTuple,
        R: IntoLuaMulti,
    {
        assert_eq!(
            params.len(),
            A::ARITY,
            "param_names length ({}) does not match arity ({}) for {}.{}",
            params.len(),
            A::ARITY,
            self.path,
            name
        );
        self.tbl.set(name, self.lua.create_function(f)?)?;
        Ok(())
    }

    pub fn lua(&self) -> &'a Lua {
        self.lua
    }

    pub fn path(&self) -> &'static str {
        self.path
    }

    pub fn tier(&self) -> Tier {
        self.tier
    }
}

/// Leak a `String` into a `&'static str`. Used at registration time only
/// (~50 calls across the whole binary), so the leaked memory is constant
/// and bounded.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}
