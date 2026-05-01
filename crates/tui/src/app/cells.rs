//! `Cells` — typed reactive name → value registry with deferred
//! subscriber notification.
//!
//! Each cell is one `Rc<dyn Any>` slot keyed by name plus a list of
//! direct subscribers; the registry also carries a flat list of
//! `glob_subs` whose `glob::Pattern` is matched against every cell
//! name on `set_dyn`. Writes (`set_dyn`) snapshot the new value and
//! the union of (direct + matching glob) subscribers into a pending
//! fire queue; the loop drains the queue at a safe point (after the
//! current `&mut Cells` / `&mut TuiApp` borrow releases) so subscriber
//! bodies can re-enter `Cells` / other subsystems freely. Same
//! "queue, release the borrow, then fire" pattern Timers and Lua
//! keymap dispatch already use.
//!
//! `Rc<dyn Any>` is intentionally `!Send`-friendly: `Cells` lives on
//! the `!Send` `TuiApp` and Lua values (carried as `mlua::RegistryKey`
//! inside `LuaCellValue`) are non-Send too. Snapshots use `Rc::clone`
//! so cell values never need a deep-clone impl — the subscriber sees
//! the value as it stood at the moment of the `set`, even if later
//! writes overwrite the slot before the drain.
//!
//! Lua-defined cells (`smelt.cell.new`) store their value as a
//! `LuaCellValue` wrapping an `mlua::RegistryKey`. Built-in Rust-typed
//! cells (`vim_mode`, `agent_mode`, …) store the typed Rust value
//! directly and rely on a per-`TypeId` `LuaProjector` registered on
//! the registry to convert `&dyn Any` to `mlua::Value` at fire time
//! (drain pump) or at read time (`smelt.cell.get`).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::rc::Rc;

use protocol::{TokenUsage, TurnMeta};

use crate::lua::LuaHandle;

/// Cell value wrapper for Lua-originated cells. Stores the value as a
/// stable `mlua::RegistryKey` so it survives Lua GC; Lua-side
/// `smelt.cell.get(name)` resolves the key back to a Lua value, and
/// the drain pump does the same when firing Lua subscribers.
/// Rust-typed built-ins carry their typed Rust value directly and
/// rely on the per-`TypeId` projector to project them into Lua at
/// fire time.
pub(crate) struct LuaCellValue {
    pub key: mlua::RegistryKey,
}

/// Per-`TypeId` converter from a stored cell value (`&dyn Any`) to a
/// Lua value. Registered with `Cells::register_lua_projector`; called
/// by the drain pump and by `Cells::get_lua` / `Cells::project_to_lua`.
/// Returns `mlua::Value::Nil` when conversion isn't possible (Lua
/// allocation failure, type mismatch).
pub(crate) type LuaProjector = Box<dyn Fn(&dyn Any, &mlua::Lua) -> mlua::Value>;

/// Stable id returned by `subscribe_kind` and consumed by
/// `unsubscribe`.
pub(crate) type SubscriptionId = u64;

/// What kind of callback to fire when a cell changes. Today only the
/// `Lua` variant ships; a Rust variant lands when the first Rust-side
/// built-in subscriber surfaces (e.g. statusline spec bindings).
#[derive(Clone)]
pub(crate) enum SubscriberKind {
    /// Handle to an `mlua::Function` stashed in the Lua registry. The
    /// drain pump resolves it against the live Lua state at fire time
    /// and projects the cell value through the per-`TypeId`
    /// `LuaProjector` registered on the registry.
    Lua(Rc<LuaHandle>),
}

struct Subscriber {
    id: SubscriptionId,
    kind: SubscriberKind,
}

struct GlobSubscriber {
    id: SubscriptionId,
    pattern: glob::Pattern,
    kind: SubscriberKind,
}

struct Slot {
    value: Rc<dyn Any>,
    subscribers: Vec<Subscriber>,
}

/// One queued callback inside a `PendingFire`. `is_glob` lets the
/// drain pump pick the right call shape: direct subscribers receive
/// `(value)`, glob subscribers receive `(name, value)` — matching
/// nvim's `pattern`-augmented autocmd ergonomics.
pub(crate) struct PendingCallback {
    pub kind: SubscriberKind,
    pub is_glob: bool,
}

/// One queued notification: the value snapshot at the moment of `set`
/// plus the subscriber callbacks captured at that moment. The caller
/// fires each callback in registration order after the `&mut Cells`
/// borrow releases.
pub(crate) struct PendingFire {
    pub name: String,
    pub value: Rc<dyn Any>,
    pub callbacks: Vec<PendingCallback>,
}

/// Typed name → value registry plus a pending-fire queue.
pub(crate) struct Cells {
    slots: HashMap<String, Slot>,
    glob_subs: Vec<GlobSubscriber>,
    pending: Vec<PendingFire>,
    next_id: SubscriptionId,
    lua_projectors: HashMap<TypeId, LuaProjector>,
}

impl Default for Cells {
    fn default() -> Self {
        Self::new()
    }
}

impl Cells {
    pub(crate) fn new() -> Self {
        let mut s = Self {
            slots: HashMap::new(),
            glob_subs: Vec::new(),
            pending: Vec::new(),
            next_id: 0,
            lua_projectors: HashMap::new(),
        };
        // Lua-defined cells: resolve the stable RegistryKey and hand
        // back the live Lua value. Without this, `smelt.cell.get`
        // and Lua subscribers would see Nil for cells created via
        // `smelt.cell.new`.
        s.register_lua_projector::<LuaCellValue, _>(|v, lua| {
            lua.registry_value::<mlua::Value>(&v.key)
                .unwrap_or(mlua::Value::Nil)
        });
        s
    }

    /// Register a converter from a stored cell value of type `T` to
    /// `mlua::Value`. The drain pump and `get_lua` use it whenever a
    /// cell's slot value is a `T` (matched by `TypeId`).
    pub(crate) fn register_lua_projector<T, F>(&mut self, project: F)
    where
        T: Any + 'static,
        F: Fn(&T, &mlua::Lua) -> mlua::Value + 'static,
    {
        let wrapper: LuaProjector = Box::new(move |any, lua| match any.downcast_ref::<T>() {
            Some(v) => project(v, lua),
            None => mlua::Value::Nil,
        });
        self.lua_projectors.insert(TypeId::of::<T>(), wrapper);
    }

    /// Project the cell value at `name` to a Lua value via the
    /// registered projector for its concrete type. Returns `Nil`
    /// when the cell isn't declared, when no projector is registered
    /// for the value's `TypeId`, or when the projector itself yields
    /// `Nil` (e.g. dropped registry key).
    pub(crate) fn get_lua(&self, name: &str, lua: &mlua::Lua) -> mlua::Value {
        let Some(slot) = self.slots.get(name) else {
            return mlua::Value::Nil;
        };
        self.project_to_lua(&*slot.value, lua)
    }

    /// Project an arbitrary `&dyn Any` through the registered
    /// projector matching its concrete type. The drain pump calls
    /// this against each pending fire's value snapshot before
    /// invoking Lua subscribers.
    pub(crate) fn project_to_lua(&self, value: &dyn Any, lua: &mlua::Lua) -> mlua::Value {
        let tid = (*value).type_id();
        match self.lua_projectors.get(&tid) {
            Some(p) => p(value, lua),
            None => mlua::Value::Nil,
        }
    }

    /// Declare a cell with its initial value. Idempotent — calling
    /// twice with the same name resets the value and drops every
    /// subscriber.
    pub(crate) fn declare<T: Any + 'static>(&mut self, name: impl Into<String>, initial: T) {
        self.slots.insert(
            name.into(),
            Slot {
                value: Rc::new(initial),
                subscribers: Vec::new(),
            },
        );
    }

    /// Overwrite the cell's value and queue every direct + matching
    /// glob subscriber for firing at the next drain. Returns `true`
    /// on success, `false` when `name` is undeclared.
    pub(crate) fn set_dyn(&mut self, name: &str, value: Rc<dyn Any>) -> bool {
        let Some(slot) = self.slots.get_mut(name) else {
            return false;
        };
        slot.value = value;
        let mut callbacks: Vec<PendingCallback> = slot
            .subscribers
            .iter()
            .map(|s| PendingCallback {
                kind: s.kind.clone(),
                is_glob: false,
            })
            .collect();
        for g in &self.glob_subs {
            if g.pattern.matches(name) {
                callbacks.push(PendingCallback {
                    kind: g.kind.clone(),
                    is_glob: true,
                });
            }
        }
        if callbacks.is_empty() {
            return true;
        }
        let snapshot = Rc::clone(&slot.value);
        self.pending.push(PendingFire {
            name: name.to_string(),
            value: snapshot,
            callbacks,
        });
        true
    }

    /// Register a subscriber callback against `name`. Returns the
    /// subscription id `unsubscribe` accepts, or `None` when `name`
    /// isn't declared.
    pub(crate) fn subscribe_kind(&mut self, name: &str, kind: SubscriberKind) -> Option<SubscriptionId> {
        let slot = self.slots.get_mut(name)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        slot.subscribers.push(Subscriber { id, kind });
        Some(id)
    }

    /// Remove the subscriber with `id` from `name`. Returns `true`
    /// if a subscriber was found and removed; `false` otherwise (cell
    /// undeclared or id unknown).
    pub(crate) fn unsubscribe(&mut self, name: &str, id: SubscriptionId) -> bool {
        let Some(slot) = self.slots.get_mut(name) else {
            return false;
        };
        let Some(idx) = slot.subscribers.iter().position(|s| s.id == id) else {
            return false;
        };
        slot.subscribers.remove(idx);
        true
    }

    /// Register a glob subscriber that fires for every cell whose
    /// name matches `pattern`. Subscribers are walked in registration
    /// order at every `set_dyn`. Returns the id `unsubscribe_glob`
    /// accepts.
    pub(crate) fn glob_subscribe(
        &mut self,
        pattern: glob::Pattern,
        kind: SubscriberKind,
    ) -> SubscriptionId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.glob_subs.push(GlobSubscriber { id, pattern, kind });
        id
    }

    /// Remove the glob subscriber with `id`. Returns `true` if a
    /// subscriber was found and removed; `false` otherwise.
    pub(crate) fn unsubscribe_glob(&mut self, id: SubscriptionId) -> bool {
        let Some(idx) = self.glob_subs.iter().position(|g| g.id == id) else {
            return false;
        };
        self.glob_subs.remove(idx);
        true
    }

    /// Pull every queued fire out of the registry. The caller invokes
    /// each `PendingFire`'s callbacks after the `&mut Cells` borrow
    /// releases. Empty when no `set_dyn` has fired since the last
    /// drain.
    pub(crate) fn drain_pending(&mut self) -> Vec<PendingFire> {
        std::mem::take(&mut self.pending)
    }

    /// Cheap probe used by the main-loop drain pump to skip the
    /// drain when nothing's pending.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Publish `value` to the cell at `name` only when it differs from
    /// the current slot. Returns `true` when a write fired (and queued
    /// subscribers); `false` when the cell is undeclared, the stored
    /// value's type doesn't match `T`, or the new value equals the old.
    /// Lets the main-loop tick fan out diff-driven cells (`vim_mode`,
    /// `confirms_pending`, …) without firing subscribers on no-op
    /// re-publishes.
    pub(crate) fn publish_if_changed<T>(&mut self, name: &str, value: T) -> bool
    where
        T: PartialEq + Any + 'static,
    {
        let Some(slot) = self.slots.get(name) else {
            return false;
        };
        if let Some(cur) = slot.value.downcast_ref::<T>() {
            if *cur == value {
                return false;
            }
        }
        self.set_dyn(name, Rc::new(value))
    }
}

/// Initial values TuiApp passes to `register_builtin_cells` so the
/// stateful cells start with the same content the underlying source
/// fields hold (mode, model, vim_mode, …) — ensures plugin authors
/// reading `smelt.cell("agent_mode"):get()` at startup see the right
/// value before any flip publishes.
pub(crate) struct BuiltinSeeds {
    pub vim_mode: String,
    pub agent_mode: String,
    pub model: String,
    pub reasoning: String,
    pub cwd: String,
    pub session_title: String,
    pub branch: String,
}

/// Sentinel placeholder for event-shaped cells whose setter hasn't
/// fired yet. The `EventStub` projector returns `nil`; the typed
/// projector (TurnMeta / TurnError / ConfirmResolved / HistoryDelta /
/// String / u64 …) takes over the moment the first `set_dyn` writes
/// the typed payload, since `Cells::project_to_lua` keys on the
/// stored value's `TypeId`, not the slot's declared type.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EventStub;

/// Payload for the `turn_error` cell. Engine emits `EngineEvent::TurnError`
/// carrying just a message; the cell projects it as a Lua table so a
/// subscriber written today still composes if the engine grows
/// structured error fields later.
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnError {
    pub message: String,
}

/// Payload for the `confirm_resolved` cell. `decision` is a stable
/// short string ("yes" | "no" | "always_session" | "always_workspace" |
/// "always_pattern_session" | "always_pattern_workspace" |
/// "always_dir_session" | "always_dir_workspace" | "auto_allow")
/// matching the resolved `ConfirmChoice` variant + scope.
#[derive(Debug, Clone)]
pub(crate) struct ConfirmResolved {
    pub handle_id: u64,
    pub decision: String,
}

/// Payload for the `history` cell. `kind` is a short stable string
/// ("set" | "cleared" | "forked" | "loaded") describing the mutation,
/// `count` is the post-mutation `messages.len()` so a subscriber sees
/// the new size without having to reach into TuiApp state.
#[derive(Debug, Clone)]
pub(crate) struct HistoryDelta {
    pub kind: String,
    pub count: usize,
}

/// Payload for the `turn_end` cell. Fires from `TuiApp::finish_turn` on
/// every turn termination — natural end (paired with `turn_complete`),
/// cancel (Esc / Ctrl-C / mode switch), or error. `cancelled = true`
/// for the cancel/error legs; subscribers needing the message list
/// query `smelt.session.messages()` from the callback.
#[derive(Debug, Clone)]
pub(crate) struct TurnEnd {
    pub cancelled: bool,
}

/// Payload for the `tool_start` cell. Fires once per tool invocation
/// at engine `ToolStarted`. Args are the same JSON-shaped map the
/// engine ships; nested objects round-trip through `json_to_lua`.
#[derive(Debug, Clone)]
pub(crate) struct ToolStart {
    pub tool: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
}

/// Payload for the `tool_end` cell. Fires on engine `ToolFinished`
/// after the result lands on the active tool entry. `is_error` mirrors
/// the engine's flag; `elapsed_ms` is `None` when the engine didn't
/// timestamp the run.
#[derive(Debug, Clone)]
pub(crate) struct ToolEnd {
    pub tool: String,
    pub is_error: bool,
    pub elapsed_ms: Option<u64>,
}

/// Payload for the `confirm_requested` cell. Carries the full request
/// snapshot the dialog needs to render — tool / desc / args / outside
/// dir / approval-pattern globs / pre-built option labels — so the Lua
/// dialog reads the request data straight from the cell instead of
/// looking it up by handle. `handle_id` keys the `_resolve` /
/// `_render_title` / `_back_tab` Rust-side primitives that still need
/// to find the underlying `Confirms` entry.
#[derive(Debug, Clone)]
pub(crate) struct ConfirmRequested {
    pub handle_id: u64,
    pub tool_name: String,
    pub desc: String,
    pub summary: Option<String>,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub outside_dir: Option<String>,
    pub approval_patterns: Vec<String>,
    pub options: Vec<String>,
    pub cwd_label: String,
}

/// Register projectors for primitive types we publish, declare every
/// built-in cell with its initial value (or an `EventStub` placeholder
/// for event-shaped cells whose payload type lands in a.4c.2/.3), and
/// return the populated `Cells` ready for subscriber registration.
pub(crate) fn build_with_builtins(seeds: BuiltinSeeds) -> Cells {
    let mut cells = Cells::new();

    // Primitive projectors covering every type a.4c.1 publishes plus
    // headroom for a.4c.2's clock + token counters.
    cells.register_lua_projector::<String, _>(|s, lua| match lua.create_string(s.as_str()) {
        Ok(s) => mlua::Value::String(s),
        Err(_) => mlua::Value::Nil,
    });
    cells.register_lua_projector::<bool, _>(|b, _| mlua::Value::Boolean(*b));
    cells.register_lua_projector::<u32, _>(|n, _| mlua::Value::Integer(*n as i64));
    cells.register_lua_projector::<u64, _>(|n, _| mlua::Value::Integer(*n as i64));
    cells.register_lua_projector::<u8, _>(|n, _| mlua::Value::Integer(*n as i64));
    // `EventStub` projector: explicit `nil`. Means a Lua subscriber
    // attached to an event-shaped cell sees `nil` until the cell's
    // setter migrates to publish a typed payload.
    cells.register_lua_projector::<EventStub, _>(|_, _| mlua::Value::Nil);
    // `TokenUsage` projector: project the typed payload into a Lua
    // table mirroring the protocol struct. `None` fields are absent
    // (no key) so plugins can `usage.prompt_tokens or 0` without
    // tripping nil-arithmetic.
    cells.register_lua_projector::<TokenUsage, _>(|u, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        if let Some(n) = u.prompt_tokens {
            let _ = t.set("prompt_tokens", n);
        }
        if let Some(n) = u.completion_tokens {
            let _ = t.set("completion_tokens", n);
        }
        if let Some(n) = u.cache_read_tokens {
            let _ = t.set("cache_read_tokens", n);
        }
        if let Some(n) = u.cache_write_tokens {
            let _ = t.set("cache_write_tokens", n);
        }
        if let Some(n) = u.reasoning_tokens {
            let _ = t.set("reasoning_tokens", n);
        }
        mlua::Value::Table(t)
    });
    // `TurnMeta` projector: surface the per-turn metadata as a flat
    // Lua table. `tool_elapsed` flattens to `{ [call_id] = ms }`;
    // `agent_blocks` is omitted today (un-migrated payload shape will
    // land alongside the agent-tools migration in P5).
    cells.register_lua_projector::<TurnMeta, _>(|m, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("elapsed_ms", m.elapsed_ms);
        if let Some(tps) = m.avg_tps {
            let _ = t.set("avg_tps", tps);
        }
        let _ = t.set("interrupted", m.interrupted);
        if let Ok(tools) = lua.create_table() {
            for (k, v) in &m.tool_elapsed {
                let _ = tools.set(k.as_str(), *v);
            }
            let _ = t.set("tool_elapsed", tools);
        }
        mlua::Value::Table(t)
    });
    // `TurnError`: `{ message = "…" }`.
    cells.register_lua_projector::<TurnError, _>(|e, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("message", e.message.as_str());
        mlua::Value::Table(t)
    });
    // `ConfirmResolved`: `{ handle_id = u64, decision = "yes" | "no" | … }`.
    cells.register_lua_projector::<ConfirmResolved, _>(|r, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("handle_id", r.handle_id);
        let _ = t.set("decision", r.decision.as_str());
        mlua::Value::Table(t)
    });
    // `HistoryDelta`: `{ kind = "set" | "cleared" | "forked" | "loaded", count = n }`.
    cells.register_lua_projector::<HistoryDelta, _>(|d, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("kind", d.kind.as_str());
        let _ = t.set("count", d.count as i64);
        mlua::Value::Table(t)
    });
    // `TurnEnd`: `{ cancelled = bool }`.
    cells.register_lua_projector::<TurnEnd, _>(|e, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("cancelled", e.cancelled);
        mlua::Value::Table(t)
    });
    // `ToolStart`: `{ tool = "...", args = {...} }`.
    cells.register_lua_projector::<ToolStart, _>(|s, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("tool", s.tool.as_str());
        if let Ok(args) = lua.create_table() {
            for (k, v) in &s.args {
                if let Ok(lv) = crate::lua::json_to_lua(lua, v) {
                    let _ = args.set(k.as_str(), lv);
                }
            }
            let _ = t.set("args", args);
        }
        mlua::Value::Table(t)
    });
    // `ToolEnd`: `{ tool = "...", is_error = bool, elapsed_ms = n? }`.
    cells.register_lua_projector::<ToolEnd, _>(|s, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("tool", s.tool.as_str());
        let _ = t.set("is_error", s.is_error);
        if let Some(n) = s.elapsed_ms {
            let _ = t.set("elapsed_ms", n);
        }
        mlua::Value::Table(t)
    });
    // `ConfirmRequested`: full request snapshot for the dialog.
    // `args` projects through `json_to_lua` so nested objects / arrays
    // round-trip into Lua tables; `outside_dir` is `nil` when absent
    // (so plugins write `if req.outside_dir then ... end`).
    cells.register_lua_projector::<ConfirmRequested, _>(|r, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("handle_id", r.handle_id);
        let _ = t.set("tool_name", r.tool_name.as_str());
        let _ = t.set("desc", r.desc.as_str());
        let _ = t.set("summary", r.summary.clone().unwrap_or_default());
        let _ = t.set("cwd_label", r.cwd_label.as_str());
        match &r.outside_dir {
            Some(s) => {
                let _ = t.set("outside_dir", s.as_str());
            }
            None => {
                let _ = t.set("outside_dir", mlua::Value::Nil);
            }
        }
        if let Ok(patterns) = lua.create_table() {
            for (i, p) in r.approval_patterns.iter().enumerate() {
                let _ = patterns.set(i + 1, p.as_str());
            }
            let _ = t.set("approval_patterns", patterns);
        }
        if let Ok(opts) = lua.create_table() {
            for (i, label) in r.options.iter().enumerate() {
                let _ = opts.set(i + 1, label.as_str());
            }
            let _ = t.set("options", opts);
        }
        if let Ok(args) = lua.create_table() {
            for (k, v) in &r.args {
                if let Ok(lv) = crate::lua::json_to_lua(lua, v) {
                    let _ = args.set(k.as_str(), lv);
                }
            }
            let _ = t.set("args", args);
        }
        mlua::Value::Table(t)
    });

    // Stateful cells (typed payloads, primitive projectors). Every
    // setter chokepoint that publishes calls `cells.set_dyn(name,
    // Rc::new(value))` after the underlying field flips.
    cells.declare("vim_mode", seeds.vim_mode);
    cells.declare("agent_mode", seeds.agent_mode);
    cells.declare("model", seeds.model);
    cells.declare("reasoning", seeds.reasoning);
    cells.declare("confirms_pending", false);
    cells.declare("tokens_used", TokenUsage::default());
    cells.declare("errors", 0u32);
    cells.declare("cwd", seeds.cwd);
    cells.declare("session_title", seeds.session_title);
    cells.declare("branch", seeds.branch);
    // `now` carries unix epoch seconds; Lua plugins format with
    // `os.date("%H:%M:%S", smelt.cell("now"):get())`. TuiApp publishes
    // through `publish_diff_cells` so subscribers fire when the
    // second changes (loop must already be awake — idle ticks
    // genuinely have nothing to display).
    cells.declare("now", 0u64);
    cells.declare("spinner_frame", 0u8);

    // Event-shaped cells: declared with an `EventStub` placeholder so
    // `smelt.cell.subscribe` works today; setters land later (turn
    // events with EngineBridge in a.11; confirm/session lifecycle as
    // their TuiApp-side handlers migrate).
    cells.declare("history", EventStub);
    cells.declare("turn_complete", EventStub);
    cells.declare("turn_error", EventStub);
    cells.declare("confirm_requested", EventStub);
    cells.declare("confirm_resolved", EventStub);
    cells.declare("session_started", EventStub);
    cells.declare("session_ended", EventStub);
    // Migrated from the parallel autocmd registry in P2.a.9. Single
    // observer mechanism: `smelt.au.on(name, fn)` and `smelt.cell:get`
    // both reach into this registry. Cells with no payload carry an
    // `EventStub` placeholder so a subscriber registered before the
    // first publish reads `nil` rather than a synthetic default.
    cells.declare("block_done", EventStub);
    cells.declare("cmd_pre", String::new());
    cells.declare("cmd_post", String::new());
    cells.declare("shutdown", EventStub);
    cells.declare("turn_start", EventStub);
    cells.declare("turn_end", EventStub);
    cells.declare("tool_start", EventStub);
    cells.declare("tool_end", EventStub);
    cells.declare("input_submit", String::new());

    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn handle(lua: &Lua, src: &str) -> Rc<LuaHandle> {
        let func: mlua::Function = lua.load(src).eval().expect("load");
        let key = lua.create_registry_value(func).expect("registry");
        Rc::new(LuaHandle { key })
    }

    #[test]
    fn declare_then_get_lua_returns_initial_value() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.register_lua_projector::<u32, _>(|n, _| mlua::Value::Integer(*n as i64));
        c.declare("count", 7u32);
        match c.get_lua("count", &lua) {
            mlua::Value::Integer(7) => {}
            other => panic!("expected Integer(7), got {other:?}"),
        }
    }

    #[test]
    fn get_lua_returns_nil_for_undeclared() {
        let lua = Lua::new();
        let c = Cells::new();
        assert!(matches!(c.get_lua("missing", &lua), mlua::Value::Nil));
    }

    #[test]
    fn set_dyn_updates_value() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.register_lua_projector::<u32, _>(|n, _| mlua::Value::Integer(*n as i64));
        c.declare("count", 0u32);
        assert!(c.set_dyn("count", Rc::new(42u32)));
        match c.get_lua("count", &lua) {
            mlua::Value::Integer(42) => {}
            other => panic!("expected Integer(42), got {other:?}"),
        }
    }

    #[test]
    fn set_dyn_returns_false_for_undeclared() {
        let mut c = Cells::new();
        assert!(!c.set_dyn("missing", Rc::new(1u32)));
    }

    #[test]
    fn set_without_subscribers_does_not_queue() {
        let mut c = Cells::new();
        c.declare("count", 0u32);
        c.set_dyn("count", Rc::new(1u32));
        assert!(!c.has_pending());
        assert_eq!(c.drain_pending().len(), 0);
    }

    #[test]
    fn subscribe_queues_fire_on_set() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        let id = c
            .subscribe_kind(
                "count",
                SubscriberKind::Lua(handle(&lua, "function(v) end")),
            )
            .expect("declared");
        assert!(id == 0); // first subscription is id 0
        c.set_dyn("count", Rc::new(5u32));
        assert!(c.has_pending());
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].name, "count");
        assert_eq!(fires[0].callbacks.len(), 1);
        assert!(!fires[0].callbacks[0].is_glob);
        // Snapshot carries the post-set value.
        assert_eq!(fires[0].value.downcast_ref::<u32>(), Some(&5u32));
    }

    #[test]
    fn multiple_subscribers_appear_in_registration_order() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        for src in [
            "function() return 1 end",
            "function() return 2 end",
            "function() return 3 end",
        ] {
            c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, src)))
                .unwrap();
        }
        c.set_dyn("count", Rc::new(1u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].callbacks.len(), 3);
    }

    #[test]
    fn unsubscribe_removes_callback() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        let id = c
            .subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        assert!(c.unsubscribe("count", id));
        c.set_dyn("count", Rc::new(1u32));
        // No subscribers, no pending fire.
        assert!(!c.has_pending());
        // Unsubscribing again is a no-op.
        assert!(!c.unsubscribe("count", id));
    }

    #[test]
    fn snapshot_carries_value_at_set_time() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.set_dyn("count", Rc::new(1u32));
        c.set_dyn("count", Rc::new(2u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].value.downcast_ref::<u32>(), Some(&1u32));
        assert_eq!(fires[1].value.downcast_ref::<u32>(), Some(&2u32));
    }

    #[test]
    fn drain_pending_is_idempotent() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.set_dyn("count", Rc::new(1u32));
        assert_eq!(c.drain_pending().len(), 1);
        assert!(c.drain_pending().is_empty());
    }

    #[test]
    fn redeclare_resets_value_and_drops_subscribers() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.register_lua_projector::<bool, _>(|b, _| mlua::Value::Boolean(*b));
        c.declare("flag", false);
        c.subscribe_kind("flag", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.declare("flag", true);
        match c.get_lua("flag", &lua) {
            mlua::Value::Boolean(true) => {}
            other => panic!("expected Boolean(true), got {other:?}"),
        }
        c.set_dyn("flag", Rc::new(false));
        // Redeclare dropped the prior subscriber.
        assert!(!c.has_pending());
    }

    #[test]
    fn subscribe_returns_none_for_undeclared() {
        let lua = Lua::new();
        let mut c = Cells::new();
        assert!(c
            .subscribe_kind(
                "missing",
                SubscriberKind::Lua(handle(&lua, "function() end"))
            )
            .is_none());
    }

    #[test]
    fn glob_subscribe_fires_for_matching_names() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("agent:1:status", "idle");
        c.declare("agent:2:status", "idle");
        c.declare("vim_mode", "Insert");
        let id = c.glob_subscribe(
            glob::Pattern::new("agent:*:status").unwrap(),
            SubscriberKind::Lua(handle(&lua, "function() end")),
        );
        // Sequential ids share the next_id counter with direct subs.
        assert!(id == 0);
        c.set_dyn("agent:1:status", Rc::new("running"));
        c.set_dyn("vim_mode", Rc::new("Normal"));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].name, "agent:1:status");
        assert_eq!(fires[0].callbacks.len(), 1);
        assert!(fires[0].callbacks[0].is_glob);
    }

    #[test]
    fn glob_and_direct_subscribers_both_fire() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("turn_complete", false);
        c.subscribe_kind(
            "turn_complete",
            SubscriberKind::Lua(handle(&lua, "function() end")),
        )
        .unwrap();
        c.glob_subscribe(
            glob::Pattern::new("turn_*").unwrap(),
            SubscriberKind::Lua(handle(&lua, "function() end")),
        );
        c.set_dyn("turn_complete", Rc::new(true));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].callbacks.len(), 2);
        // Direct subscriber appears before glob in registration order.
        assert!(!fires[0].callbacks[0].is_glob);
        assert!(fires[0].callbacks[1].is_glob);
    }

    #[test]
    fn unsubscribe_glob_removes_callback() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("foo", 0u32);
        let id = c.glob_subscribe(
            glob::Pattern::new("*").unwrap(),
            SubscriberKind::Lua(handle(&lua, "function() end")),
        );
        assert!(c.unsubscribe_glob(id));
        c.set_dyn("foo", Rc::new(1u32));
        assert!(!c.has_pending());
        // Unsubscribing again is a no-op.
        assert!(!c.unsubscribe_glob(id));
    }

    #[test]
    fn glob_subscriber_does_not_fire_for_undeclared_name() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.glob_subscribe(
            glob::Pattern::new("*").unwrap(),
            SubscriberKind::Lua(handle(&lua, "function() end")),
        );
        // No declared cell, so set_dyn returns false and queues nothing.
        assert!(!c.set_dyn("missing", Rc::new(1u32)));
        assert!(!c.has_pending());
    }

    #[test]
    fn lua_cell_value_round_trip() {
        let lua = Lua::new();
        let value: mlua::Value = lua.load("\"hello\"").eval().unwrap();
        let key = lua.create_registry_value(value).unwrap();
        let mut c = Cells::new();
        c.declare("greeting", LuaCellValue { key });
        match c.get_lua("greeting", &lua) {
            mlua::Value::String(s) => {
                assert_eq!(s.to_str().unwrap(), "hello");
            }
            other => panic!("expected String(hello), got {other:?}"),
        }
    }

    #[test]
    fn builtin_seeds_declare_every_cell() {
        let lua = Lua::new();
        let cells = build_with_builtins(BuiltinSeeds {
            vim_mode: "Insert".into(),
            agent_mode: "normal".into(),
            model: "anthropic/claude-opus-4-7".into(),
            reasoning: "off".into(),
            cwd: "/tmp/work".into(),
            session_title: String::new(),
            branch: String::new(),
        });

        // Stateful cells with primitive projectors return their seeds.
        for (name, expected) in [
            ("vim_mode", "Insert"),
            ("agent_mode", "normal"),
            ("model", "anthropic/claude-opus-4-7"),
            ("reasoning", "off"),
            ("cwd", "/tmp/work"),
        ] {
            match cells.get_lua(name, &lua) {
                mlua::Value::String(s) => assert_eq!(s.to_str().unwrap(), expected),
                other => panic!("cell {name}: expected String({expected}), got {other:?}"),
            }
        }

        // Event-shaped cells project to nil while their setters are
        // un-migrated.
        for name in [
            "history",
            "turn_complete",
            "turn_error",
            "confirm_requested",
            "confirm_resolved",
            "session_started",
            "session_ended",
        ] {
            assert!(
                matches!(cells.get_lua(name, &lua), mlua::Value::Nil),
                "cell {name} should project to Nil"
            );
        }

        // `now` initialises at 0 (epoch); `spinner_frame` at 0; both
        // project as Lua integers via the u64 / u8 projectors.
        assert!(matches!(
            cells.get_lua("now", &lua),
            mlua::Value::Integer(0)
        ));
        assert!(matches!(
            cells.get_lua("spinner_frame", &lua),
            mlua::Value::Integer(0)
        ));

        // `tokens_used` initialises as `TokenUsage::default()` whose
        // every field is `None`; the projector returns an empty table.
        match cells.get_lua("tokens_used", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.len().unwrap(), 0);
                assert_eq!(t.pairs::<String, i64>().count(), 0);
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn token_usage_projector_emits_named_fields() {
        let lua = Lua::new();
        let mut c = Cells::new();
        // The TokenUsage projector lives in build_with_builtins; mirror
        // the registration here so the unit test is hermetic.
        c.register_lua_projector::<TokenUsage, _>(|u, lua| {
            let Ok(t) = lua.create_table() else {
                return mlua::Value::Nil;
            };
            if let Some(n) = u.prompt_tokens {
                let _ = t.set("prompt_tokens", n);
            }
            if let Some(n) = u.completion_tokens {
                let _ = t.set("completion_tokens", n);
            }
            mlua::Value::Table(t)
        });
        c.declare(
            "tokens_used",
            TokenUsage {
                prompt_tokens: Some(1234),
                completion_tokens: Some(456),
                ..Default::default()
            },
        );
        match c.get_lua("tokens_used", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("prompt_tokens").unwrap(), 1234);
                assert_eq!(t.get::<i64>("completion_tokens").unwrap(), 456);
                // Absent fields surface as nil — not 0 — so plugins can
                // distinguish "no data" from "0 tokens".
                assert!(matches!(
                    t.get::<mlua::Value>("reasoning_tokens").unwrap(),
                    mlua::Value::Nil
                ));
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn event_payload_projectors_emit_named_fields() {
        let lua = Lua::new();
        let cells = build_with_builtins(BuiltinSeeds {
            vim_mode: "Insert".into(),
            agent_mode: "normal".into(),
            model: "m".into(),
            reasoning: "off".into(),
            cwd: "/".into(),
            session_title: String::new(),
            branch: String::new(),
        });

        // Set typed payloads via set_dyn — Cells::project_to_lua keys
        // on the stored value's TypeId, so the typed projector takes
        // over even though the slot was declared with EventStub.
        let mut cells = cells;
        let mut tool_elapsed = std::collections::HashMap::new();
        tool_elapsed.insert("call_42".to_string(), 1500u64);
        cells.set_dyn(
            "turn_complete",
            Rc::new(TurnMeta {
                elapsed_ms: 12000,
                avg_tps: Some(33.5),
                interrupted: false,
                tool_elapsed,
                agent_blocks: std::collections::HashMap::new(),
            }),
        );
        cells.set_dyn(
            "turn_error",
            Rc::new(TurnError {
                message: "boom".into(),
            }),
        );
        cells.set_dyn(
            "confirm_resolved",
            Rc::new(ConfirmResolved {
                handle_id: 7,
                decision: "always_session".into(),
            }),
        );
        cells.set_dyn(
            "history",
            Rc::new(HistoryDelta {
                kind: "set".into(),
                count: 4,
            }),
        );
        cells.set_dyn("session_started", Rc::new(String::from("sess-001")));
        cells.set_dyn(
            "confirm_requested",
            Rc::new(ConfirmRequested {
                handle_id: 42,
                tool_name: "bash".into(),
                desc: "ls".into(),
                summary: None,
                args: std::collections::HashMap::new(),
                outside_dir: None,
                approval_patterns: vec!["bash:ls".into()],
                options: vec!["yes".into(), "no".into()],
                cwd_label: "~/work".into(),
            }),
        );

        match cells.get_lua("turn_complete", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("elapsed_ms").unwrap(), 12000);
                assert!((t.get::<f64>("avg_tps").unwrap() - 33.5).abs() < f64::EPSILON);
                assert!(!t.get::<bool>("interrupted").unwrap());
                let tools: mlua::Table = t.get("tool_elapsed").unwrap();
                assert_eq!(tools.get::<i64>("call_42").unwrap(), 1500);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match cells.get_lua("turn_error", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<String>("message").unwrap(), "boom");
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match cells.get_lua("confirm_resolved", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("handle_id").unwrap(), 7);
                assert_eq!(t.get::<String>("decision").unwrap(), "always_session");
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match cells.get_lua("history", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<String>("kind").unwrap(), "set");
                assert_eq!(t.get::<i64>("count").unwrap(), 4);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match cells.get_lua("session_started", &lua) {
            mlua::Value::String(s) => assert_eq!(s.to_str().unwrap(), "sess-001"),
            other => panic!("expected String, got {other:?}"),
        }
        match cells.get_lua("confirm_requested", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("handle_id").unwrap(), 42);
                assert_eq!(t.get::<String>("tool_name").unwrap(), "bash");
                assert_eq!(t.get::<String>("desc").unwrap(), "ls");
                assert_eq!(t.get::<String>("cwd_label").unwrap(), "~/work");
                let opts: mlua::Table = t.get("options").unwrap();
                assert_eq!(opts.get::<String>(1).unwrap(), "yes");
                assert_eq!(opts.get::<String>(2).unwrap(), "no");
                let patterns: mlua::Table = t.get("approval_patterns").unwrap();
                assert_eq!(patterns.get::<String>(1).unwrap(), "bash:ls");
                assert!(matches!(
                    t.get::<mlua::Value>("outside_dir").unwrap(),
                    mlua::Value::Nil
                ));
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }
}
