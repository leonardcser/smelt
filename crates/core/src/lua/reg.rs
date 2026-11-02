//! `smelt.Reg` - uniform registration handle.
//!
//! Every reactive-subscription API (`win:key`, `win:on`, `timer.set`,
//! `timer.every`, `cell:subscribe`, `cell.glob`) returns a `Reg`. The
//! handle carries a type-erased undoer that fires exactly once on
//! `:remove()`; subsequent calls return `false`.

use crate::lua::lua_type::LuaType;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

/// Boxed `FnOnce() -> bool` undoer, parked in a `RefCell` so `:remove()`
/// can take it on first call.
type Undoer = Box<dyn FnOnce() -> bool>;
type SharedUndoer = Rc<RefCell<Option<Undoer>>>;

#[derive(Default)]
struct Scope {
    id: u64,
    undoers: Vec<SharedUndoer>,
    resources: HashSet<(&'static str, u64)>,
}

thread_local! {
    static SCOPES: RefCell<Vec<Scope>> = const { RefCell::new(Vec::new()) };
    static NEXT_SCOPE: Cell<u64> = const { Cell::new(0) };
}

/// Synchronous construction transaction. Only active factories pay for shared
/// undoers; ordinary registration handles retain their single allocation.
/// Never hold a scope across a yield or an event-loop boundary.
pub struct RegistrationScope {
    active: bool,
    _thread: std::marker::PhantomData<Rc<()>>,
}

impl Default for RegistrationScope {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistrationScope {
    pub fn new() -> Self {
        let id = NEXT_SCOPE.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        SCOPES.with_borrow_mut(|scopes| {
            scopes.push(Scope {
                id,
                ..Scope::default()
            })
        });
        Self {
            active: true,
            _thread: std::marker::PhantomData,
        }
    }

    pub fn active() -> bool {
        SCOPES.with_borrow(|scopes| !scopes.is_empty())
    }

    pub fn owns(kind: &'static str, id: u64) -> bool {
        SCOPES.with_borrow(|scopes| {
            scopes
                .last()
                .is_some_and(|scope| scope.resources.contains(&(kind, id)))
        })
    }

    pub fn resource(kind: &'static str, id: u64, undo: impl FnOnce() -> bool + 'static) {
        let fresh = SCOPES.with_borrow_mut(|scopes| {
            scopes
                .last_mut()
                .is_some_and(|scope| scope.resources.insert((kind, id)))
        });
        if fresh {
            LuaReg::new(undo);
        }
    }

    /// Factory-local names cannot reopen resources owned by other views or
    /// older Lua generations. Repeated names within one factory still work.
    pub fn name(name: Option<String>) -> Option<String> {
        name.map(|name| {
            SCOPES.with_borrow(|scopes| match scopes.last() {
                Some(scope) => format!("view:{}:{name}", scope.id),
                None => name,
            })
        })
    }

    /// Commit construction and return its one-shot cleanup. Creating it after
    /// popping enrolls nested owners in their enclosing construction scope.
    pub fn finish(mut self) -> LuaReg {
        let scope = SCOPES.with_borrow_mut(|scopes| {
            let mut scope = scopes.pop().expect("construction scope");
            let resources = std::mem::take(&mut scope.resources);
            if let Some(parent) = scopes.last_mut() {
                parent.resources.extend(resources);
            }
            scope
        });
        self.active = false;
        LuaReg::new(move || {
            release(scope);
            true
        })
    }
}

fn release(scope: Scope) {
    for undoer in scope.undoers.into_iter().rev() {
        let undo = undoer.borrow_mut().take();
        if let Some(undo) = undo {
            undo();
        }
    }
}

impl Drop for RegistrationScope {
    fn drop(&mut self) {
        if self.active {
            let scope = SCOPES.with_borrow_mut(|scopes| scopes.pop().expect("construction scope"));
            release(scope);
        }
    }
}

/// Userdata wrapping a one-shot undoer. Constructed by Rust callers via
/// [`LuaReg::new`] and yielded to Lua as `smelt.Reg`.
pub struct LuaReg {
    undoer: RefCell<Option<Undoer>>,
}

impl LuaReg {
    pub fn new<F: FnOnce() -> bool + 'static>(f: F) -> Self {
        let mut undoer: Undoer = Box::new(f);
        if RegistrationScope::active() {
            let shared = Rc::new(RefCell::new(Some(undoer)));
            SCOPES.with_borrow_mut(|scopes| {
                scopes
                    .last_mut()
                    .expect("construction scope")
                    .undoers
                    .push(shared.clone())
            });
            undoer = Box::new(move || {
                let undo = shared.borrow_mut().take();
                undo.is_some_and(|undo| undo())
            });
        }
        Self {
            undoer: RefCell::new(Some(undoer)),
        }
    }

    /// For shared registrations whose callers already own scoped cleanup.
    pub(crate) fn unscoped(f: impl FnOnce() -> bool + 'static) -> Self {
        Self {
            undoer: RefCell::new(Some(Box::new(f))),
        }
    }

    pub fn remove(&self) -> bool {
        let undoer = self.undoer.borrow_mut().take();
        undoer.is_some_and(|undo| undo())
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
        methods.add_method("remove", |_, this, ()| Ok(this.remove()));
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
