//! Generic hook registry — the storage and lifecycle pattern shared by
//! tool middleware (`smelt.tools.middleware`), provider middleware
//! (`smelt.provider.middleware`), and any future "register a callback
//! list" surface. Each registered hook gets a monotonic id from the
//! owning registry; `off()` removes by id, so plugins composing
//! interleaved registrations never accidentally remove each other's
//! handlers.
//!
//! Hooks are name-scoped: `name = ""` registers a wildcard that fires
//! for every dispatched event; a non-empty name only fires when the
//! caller-supplied filter matches exactly. The empty-string convention
//! keeps the surface flat — there's no separate "global" vs
//! "per-target" registration; one API does both.

use super::LuaHandle;
use mlua::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Single hook entry. `id` is unique within its owning `HookRegistry`;
/// the same numeric value may exist in a different registry without
/// meaning anything to it. `name = ""` matches any dispatch filter.
pub struct HookEntry {
    pub id: u64,
    pub name: String,
    pub handle: LuaHandle,
}

/// Append-only registry of Lua-callable hooks. Cheap to clone (`Arc`)
/// and `Send + Sync` so consumers in tokio tasks can hold a reference
/// across `.await` points.
#[derive(Default)]
pub struct HookRegistry {
    next_id: AtomicU64,
    entries: Mutex<Vec<HookEntry>>,
}

impl HookRegistry {
    /// Allocate a fresh registry. `Default` works too — both forms exist
    /// so consumers can drop one in a `lazy_static`-style site without
    /// importing the trait.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a Lua function under `name`. Returns the freshly-minted id
    /// for use with [`off_for`] or [`remove`]. The caller is responsible
    /// for stashing the returned id when constructing composite `off()`
    /// closures (e.g. `tools.middleware` registers in two registries
    /// under one user-visible handle).
    pub fn register(
        &self,
        lua: &Lua,
        func: mlua::Function,
        name: impl Into<String>,
    ) -> LuaResult<u64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handle = LuaHandle::from_func(lua, func)?;
        if let Ok(mut v) = self.entries.lock() {
            v.push(HookEntry {
                id,
                name: name.into(),
                handle,
            });
        }
        Ok(id)
    }

    /// Build a Lua `off()` function that removes entry `id` from this
    /// registry exactly. The returned function captures an
    /// `Arc<HookRegistry>` so it's safe to call from any phase, even
    /// after the original registration site is long gone.
    pub fn off_for(self: &Arc<Self>, lua: &Lua, id: u64) -> LuaResult<mlua::Function> {
        let me = Arc::clone(self);
        lua.create_function(move |_, ()| -> LuaResult<bool> { Ok(me.remove(id)) })
    }

    /// Remove the entry whose id matches. Returns `true` when an entry
    /// was removed. Idempotent.
    pub fn remove(&self, id: u64) -> bool {
        if let Ok(mut v) = self.entries.lock() {
            let before = v.len();
            v.retain(|e| e.id != id);
            return v.len() != before;
        }
        false
    }

    /// Snapshot the Lua functions whose entry name is `""` or equals
    /// `name`, in registration order. Returns an owned vector so the
    /// mutex is released before the caller invokes the functions —
    /// preventing re-entrancy deadlocks when a hook reads back from the
    /// same registry. Entries stay registered for the next dispatch.
    pub fn snapshot_for(&self, lua: &Lua, name: &str) -> Vec<mlua::Function> {
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        entries
            .iter()
            .filter(|e| e.name.is_empty() || e.name == name)
            .filter_map(|e| lua.registry_value::<mlua::Function>(&e.handle.key).ok())
            .collect()
    }

    /// One-shot variant of [`snapshot_for`]: take every entry whose name
    /// matches (including the `""` wildcard), drop them from the registry,
    /// and return the cloned Lua functions in registration order. Use this
    /// for events that fire once per launch (lifecycle hooks) so the same
    /// callback can't re-fire after a `/reload` re-registration.
    pub fn drain_for(&self, lua: &Lua, name: &str) -> Vec<mlua::Function> {
        let Ok(mut entries) = self.entries.lock() else {
            return Vec::new();
        };
        let mut drained = Vec::new();
        entries.retain(|e| {
            let matches = e.name.is_empty() || e.name == name;
            if matches {
                if let Ok(f) = lua.registry_value::<mlua::Function>(&e.handle.key) {
                    drained.push(f);
                }
                false
            } else {
                true
            }
        });
        drained
    }

    /// `true` when no entries are registered. Cheap check used by hot
    /// paths (e.g. provider request hooks) to skip building call-site
    /// machinery when nobody is listening.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().map(|v| v.is_empty()).unwrap_or(true)
    }

    /// Drop every entry. Used by `/reload`.
    pub fn clear(&self) {
        if let Ok(mut v) = self.entries.lock() {
            v.clear();
        }
    }
}

/// Build a composite `off()` that removes one id from each of several
/// registries. Used by surfaces (like `smelt.tools.middleware`) whose
/// user-visible handle straddles multiple registries.
pub fn composite_off(lua: &Lua, parts: Vec<(Arc<HookRegistry>, u64)>) -> LuaResult<mlua::Function> {
    lua.create_function(move |_, ()| -> LuaResult<bool> {
        let mut any = false;
        for (reg, id) in &parts {
            any |= reg.remove(*id);
        }
        Ok(any)
    })
}
