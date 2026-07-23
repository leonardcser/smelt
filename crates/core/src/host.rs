//! Scoped Core access for Lua bindings.
//!
//! A frontend lends its `Core` to Lua only for the dynamic extent of one Lua
//! entry. The scoped slot carries the borrow lifetime, so callbacks cannot
//! retain host authority or alias an ordinary Rust borrow.

use super::runtime::Core;
use scoped_tls_hkt::scoped_thread_local;

/// Exclusive access to the core capability during one Lua entry.
///
/// The callback form lets a frontend delegate Core access without exposing or
/// downcasting its frontend root. The callback must run synchronously and may
/// not retain the borrowed Core.
pub trait LuaHost {
    fn with_core(&mut self, callback: &mut dyn FnMut(&mut Core));
}

impl LuaHost for Core {
    fn with_core(&mut self, callback: &mut dyn FnMut(&mut Core)) {
        callback(self);
    }
}

scoped_thread_local!(static mut HOST: for<'a> &'a mut dyn LuaHost);

/// Lend one frontend root to Lua for the duration of `body`.
pub fn scope_host<R>(host: &mut dyn LuaHost, body: impl FnOnce() -> R) -> R {
    HOST.set(host, body)
}

/// Lend a standalone Core to Lua for the duration of `body`.
pub fn scope_core<R>(core: &mut Core, body: impl FnOnce() -> R) -> R {
    scope_host(core, body)
}

/// Return whether this thread is currently executing a scoped Lua entry.
///
/// Frontends use this to defer work that would require an ordinary mutable host
/// borrow until the active Lua callback returns.
pub fn host_access_active() -> bool {
    HOST.is_set()
}

/// Borrow the Core currently lent to Lua.
///
/// Panics when called outside [`scope_host`] or [`scope_core`].
pub fn with_core<R>(callback: impl FnOnce(&mut Core) -> R) -> R {
    let mut callback = Some(callback);
    let mut result = None;
    HOST.with(|host| {
        host.with_core(&mut |core| {
            let callback = callback
                .take()
                .expect("Lua Core callback ran more than once");
            result = Some(callback(core));
        });
    });
    result.expect("Lua host did not provide Core access")
}

/// Borrow the Core currently lent to Lua, or return `None` outside a Lua entry.
pub fn try_with_core<R>(callback: impl FnOnce(&mut Core) -> R) -> Option<R> {
    HOST.is_set().then(|| with_core(callback))
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_is_unavailable_outside_a_scoped_lua_entry() {
        assert!(super::try_with_core(|_| ()).is_none());
    }
}
