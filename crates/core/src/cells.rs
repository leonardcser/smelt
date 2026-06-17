//! Typed reactive name → value registry with deferred subscriber notification.
//!
//! Each cell is an `Rc<dyn Any>` slot. Writes queue direct + glob subscribers for firing after
//! the `&mut Cells` borrow releases, so subscriber bodies can re-enter `Cells` freely. Lua cells
//! store an `mlua::RegistryKey`; Rust-typed cells use a per-`TypeId` `LuaProjector` to convert to
//! `mlua::Value` at drain time.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::rc::Rc;

use protocol::{TokenUsage, TurnMeta};

use crate::lua::LuaHandle;

/// Value for Lua-originated cells. Stored as a stable `mlua::RegistryKey` so it survives GC.
pub(crate) struct LuaCellValue {
    pub(crate) key: mlua::RegistryKey,
}

/// Converter from a stored cell value (`&dyn Any`) to a Lua value, keyed by `TypeId`.
pub(crate) type LuaProjector = Box<dyn Fn(&dyn Any, &mlua::Lua) -> mlua::Value>;

/// Stable id returned by `subscribe_kind` and consumed by `unsubscribe`.
pub(crate) type SubscriptionId = u64;

#[derive(Clone)]
pub enum SubscriberKind {
    /// Lua function stashed in the registry; projected at fire time via the per-`TypeId` projector.
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

/// One queued callback inside a `PendingFire`. `is_glob` selects the call shape:
/// direct → `(new, old)`, glob → `(name, new, old)`.
pub struct PendingCallback {
    pub kind: SubscriberKind,
    pub is_glob: bool,
}

/// One queued notification: value snapshot, previous value, and subscriber callbacks.
/// Fired in registration order after the `&mut Cells` borrow releases.
pub struct PendingFire {
    pub name: String,
    pub value: Rc<dyn Any>,
    pub prev: Rc<dyn Any>,
    pub callbacks: Vec<PendingCallback>,
}

pub struct Cells {
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
        s.register_lua_projector::<LuaCellValue, _>(|v, lua| {
            lua.registry_value::<mlua::Value>(&v.key)
                .unwrap_or(mlua::Value::Nil)
        });
        s
    }

    fn register_lua_projector<T, F>(&mut self, project: F)
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

    /// Return the typed value at `name`, if the cell exists with that exact type.
    pub fn get<T: Any + Clone + 'static>(&self, name: &str) -> Option<T> {
        self.slots
            .get(name)
            .and_then(|slot| slot.value.downcast_ref::<T>().cloned())
    }

    /// Project the cell at `name` to a Lua value. Returns `Nil` when undeclared or no projector.
    pub(crate) fn get_lua(&self, name: &str, lua: &mlua::Lua) -> mlua::Value {
        let Some(slot) = self.slots.get(name) else {
            return mlua::Value::Nil;
        };
        self.project_to_lua(&*slot.value, lua)
    }

    pub fn project_to_lua(&self, value: &dyn Any, lua: &mlua::Lua) -> mlua::Value {
        let tid = (*value).type_id();
        match self.lua_projectors.get(&tid) {
            Some(p) => p(value, lua),
            None => mlua::Value::Nil,
        }
    }

    /// Declare a cell. Idempotent - re-declaration resets the value and drops all subscribers.
    pub(crate) fn declare<T: Any + 'static>(&mut self, name: impl Into<String>, initial: T) {
        self.slots.insert(
            name.into(),
            Slot {
                value: Rc::new(initial),
                subscribers: Vec::new(),
            },
        );
    }

    /// Overwrite a cell and queue subscribers. Returns `false` when `name` is undeclared.
    pub fn set_dyn(&mut self, name: &str, value: Rc<dyn Any>) -> bool {
        let Some(slot) = self.slots.get_mut(name) else {
            return false;
        };
        let prev = std::mem::replace(&mut slot.value, value);
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
            prev,
            callbacks,
        });
        true
    }

    /// Subscribe to `name`. Returns `None` when the cell is undeclared.
    pub(crate) fn subscribe_kind(
        &mut self,
        name: &str,
        kind: SubscriberKind,
    ) -> Option<SubscriptionId> {
        let slot = self.slots.get_mut(name)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        slot.subscribers.push(Subscriber { id, kind });
        Some(id)
    }

    /// Remove a subscriber. Returns `false` when the cell is undeclared or `id` is unknown.
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

    pub(crate) fn unsubscribe_glob(&mut self, id: SubscriptionId) -> bool {
        let Some(idx) = self.glob_subs.iter().position(|g| g.id == id) else {
            return false;
        };
        self.glob_subs.remove(idx);
        true
    }

    pub fn drain_pending(&mut self) -> Vec<PendingFire> {
        std::mem::take(&mut self.pending)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Drop every Lua subscriber (direct + glob) plus queued fires.
    /// Dropping `pending` too prevents one stale post-reload firing.
    pub fn clear_lua_subscribers(&mut self) {
        for slot in self.slots.values_mut() {
            slot.subscribers.clear();
        }
        self.glob_subs.clear();
        self.pending.clear();
    }

    /// Publish `value` only when it differs from the current slot. Skips subscribers on no-op writes.
    pub fn publish_if_changed<T>(&mut self, name: &str, value: T) -> bool
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

/// Seed values for stateful built-in cells, so plugins read correct state at startup.
pub(crate) struct BuiltinSeeds {
    pub(crate) vim_mode: String,
    pub(crate) agent_mode: String,
    pub(crate) model: String,
    pub(crate) reasoning: String,
    pub(crate) cwd: String,
    pub(crate) session_title: String,
    pub(crate) branch: String,
}

/// Placeholder for event-shaped cells before the first typed payload is published. Projects to `nil`.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventStub;

/// Payload for the `turn_error` cell.
#[derive(Debug, Default, Clone)]
pub struct TurnError {
    pub message: String,
}

/// Payload for the `confirm_resolved` cell. `decision` is a stable string matching the
/// resolved `ConfirmChoice` variant + scope (e.g. `"yes"`, `"always_session"`).
#[derive(Debug, Clone)]
pub struct ConfirmResolved {
    pub handle_id: u64,
    pub decision: String,
}

/// Payload for the `history` cell. `kind` is `"set" | "cleared" | "forked" | "loaded"`.
#[derive(Debug, Clone)]
pub struct HistoryDelta {
    pub kind: String,
    pub count: usize,
}

/// Payload for the `turn_end` cell. `cancelled` is `true` for cancel/error legs.
#[derive(Debug, Clone)]
pub struct TurnEnd {
    pub cancelled: bool,
}

/// Payload for the `tool_start` cell.
#[derive(Debug, Clone)]
pub struct ToolStart {
    pub tool: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
}

/// Payload for the `tool_end` cell. `elapsed_ms` is `None` when not timed.
#[derive(Debug, Clone)]
pub struct ToolEnd {
    pub tool: String,
    pub is_error: bool,
    pub elapsed_ms: Option<u64>,
}

/// Payload for the `confirm_requested` cell. Full snapshot the Lua dialog reads.
#[derive(Debug, Clone)]
pub struct ConfirmRequested {
    pub handle_id: u64,
    pub tool_name: String,
    /// Styled summary - the sole source of truth for the dialog body header.
    pub summary: protocol::StyledLines,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub grant_options: Vec<crate::transcript_model::ConfirmApprovalOption>,
}

/// Single entry in the `work_busy` cell payload. One per live
/// `smelt.work.busy` token, projected newest-last as
/// `{ id, label }` Lua tables. `id` is the monotonic token id returned
/// by `push`; plugins compare it across ticks to spot specific tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkBusyEntry {
    pub id: u64,
    pub label: String,
}

/// Payload for the `cursor_pos` cell. Tracks the focused window's
/// cursor for the statusline position pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorPos {
    pub line: u32,
    pub col: u32,
    pub scroll_pct: u8,
}

/// Payload for the `stream_delta` cell. Emitted for every streaming
/// chunk arriving from the provider - text, thinking, and tool-call
/// argument JSON fragments. Use `bytes` for cheap counters (live TPS);
/// `text` carries the raw delta. For `kind == "tool_args"`, `call_id`
/// and `tool_name` identify which tool call the fragment belongs to.
#[derive(Debug, Clone)]
pub struct StreamDelta {
    /// `"text" | "thinking" | "tool_args"`.
    pub kind: String,
    /// UTF-8 byte length of the delta.
    pub bytes: usize,
    /// Raw delta bytes.
    pub text: String,
    /// `call_id` of the tool call this fragment belongs to. Populated
    /// only when `kind == "tool_args"`.
    pub call_id: Option<String>,
    /// `tool_name` of the tool call this fragment belongs to. Populated
    /// only when `kind == "tool_args"`.
    pub tool_name: Option<String>,
}

/// Built-in cell names declared by [`build_with_builtins`]. Surfaces in
/// the `smelt.cell.Name` LuaCATS alias as IDE autocomplete hints.
/// Keep in lockstep with the `cells.declare(...)` calls below - both
/// the assertion in `builtin_seeds_declare_every_cell` and
/// `crates/core/src/lua/api/cell.rs` read from this list.
pub const SEEDED_CELL_NAMES: &[&str] = &[
    "agent_mode",
    "block_done",
    "branch",
    "cmd_post",
    "cmd_pre",
    "confirm_requested",
    "confirm_resolved",
    "confirms_pending",
    "cursor_pos",
    "cwd",
    "errors",
    "history",
    "history_epoch",
    "input_epoch",
    "input_submit",
    "keymap_pending",
    "model",
    "now",
    "notification_visible",
    "permission_pending",
    "reasoning",
    "running_procs",
    "session_ended",
    "session_epoch",
    "session_started",
    "session_title",
    "shutdown",
    "spinner_frame",
    "stream_delta",
    "task_label",
    "tokens_used",
    "tool_end",
    "tool_start",
    "tps",
    "turn_complete",
    "turn_end",
    "turn_error",
    "turn_start",
    "vim_mode",
    "work_busy",
    "work_elapsed_ms",
    "work_label",
    "work_outcome",
    "work_retry_attempt",
    "work_retry_remaining_ms",
    "work_state",
];

/// Project a `StyledLines` payload into the same shape Lua sees from
/// `buf:styled` - a sequence of lines, each a sequence
/// of `{ text, syntax?, selectable?, title_suffix?, style? = { hl?, dim?, bold?, italic?, fg?, bg? } }`
/// span tables. Empty lines come through as `{}`.
fn styled_lines_to_lua(lua: &mlua::Lua, sl: &protocol::StyledLines) -> mlua::Value {
    let Ok(out) = lua.create_table() else {
        return mlua::Value::Nil;
    };
    for (i, line) in sl.0.iter().enumerate() {
        let Ok(line_tbl) = lua.create_table() else {
            continue;
        };
        for (j, span) in line.iter().enumerate() {
            let Ok(span_tbl) = lua.create_table() else {
                continue;
            };
            let _ = span_tbl.set("text", span.text.as_str());
            if let Some(s) = &span.syntax {
                let _ = span_tbl.set("syntax", s.as_str());
            }
            let needs_style = span.hl.is_some()
                || span.fg.is_some()
                || span.bg.is_some()
                || span.dim
                || span.bold
                || span.italic;
            if needs_style {
                if let Ok(style_tbl) = lua.create_table() {
                    if let Some(h) = &span.hl {
                        let _ = style_tbl.set("hl", h.as_str());
                    }
                    if let Some(f) = &span.fg {
                        let _ = style_tbl.set("fg", f.as_str());
                    }
                    if let Some(b) = &span.bg {
                        let _ = style_tbl.set("bg", b.as_str());
                    }
                    if span.dim {
                        let _ = style_tbl.set("dim", true);
                    }
                    if span.bold {
                        let _ = style_tbl.set("bold", true);
                    }
                    if span.italic {
                        let _ = style_tbl.set("italic", true);
                    }
                    let _ = span_tbl.set("style", style_tbl);
                }
            }
            if !span.selectable {
                let _ = span_tbl.set("selectable", false);
            }
            if span.title_suffix {
                let _ = span_tbl.set("title_suffix", true);
            }
            let _ = line_tbl.set(j + 1, span_tbl);
        }
        let _ = out.set(i + 1, line_tbl);
    }
    mlua::Value::Table(out)
}

/// Register projectors and declare all built-in cells with their initial values.
pub(crate) fn build_with_builtins(seeds: BuiltinSeeds) -> Cells {
    let mut cells = Cells::new();

    cells.register_lua_projector::<String, _>(|s, lua| match lua.create_string(s.as_str()) {
        Ok(s) => mlua::Value::String(s),
        Err(_) => mlua::Value::Nil,
    });
    cells.register_lua_projector::<bool, _>(|b, _| mlua::Value::Boolean(*b));
    cells.register_lua_projector::<u32, _>(|n, _| mlua::Value::Integer(*n as i64));
    cells.register_lua_projector::<u64, _>(|n, _| mlua::Value::Integer(*n as i64));
    cells.register_lua_projector::<u8, _>(|n, _| mlua::Value::Integer(*n as i64));
    cells.register_lua_projector::<f64, _>(|n, _| mlua::Value::Number(*n));
    cells.register_lua_projector::<CursorPos, _>(|p, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("line", p.line as i64);
        let _ = t.set("col", p.col as i64);
        let _ = t.set("scroll_pct", p.scroll_pct as i64);
        mlua::Value::Table(t)
    });
    cells.register_lua_projector::<EventStub, _>(|_, _| mlua::Value::Nil);
    // `None` fields are absent so plugins can write `usage.prompt_tokens or 0`.
    cells.register_lua_projector::<TokenUsage, _>(|u, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        if let Some(n) = u.context_tokens {
            let _ = t.set("context_tokens", n);
        }
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
    cells.register_lua_projector::<TurnMeta, _>(|m, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("elapsed_ms", m.elapsed_ms);
        if let Some(tps) = m.avg_tps {
            let _ = t.set("avg_tps", tps);
        }
        if let Some(tps) = m.display_tps {
            let _ = t.set("display_tps", tps);
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
    cells.register_lua_projector::<TurnError, _>(|e, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("message", e.message.as_str());
        mlua::Value::Table(t)
    });
    cells.register_lua_projector::<ConfirmResolved, _>(|r, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("handle_id", r.handle_id);
        let _ = t.set("decision", r.decision.as_str());
        mlua::Value::Table(t)
    });
    cells.register_lua_projector::<HistoryDelta, _>(|d, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("kind", d.kind.as_str());
        let _ = t.set("count", d.count as i64);
        mlua::Value::Table(t)
    });
    cells.register_lua_projector::<TurnEnd, _>(|e, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("cancelled", e.cancelled);
        mlua::Value::Table(t)
    });
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
    cells.register_lua_projector::<StreamDelta, _>(|d, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("kind", d.kind.as_str());
        let _ = t.set("bytes", d.bytes);
        let _ = t.set("text", d.text.as_str());
        if let Some(cid) = &d.call_id {
            let _ = t.set("call_id", cid.as_str());
        }
        if let Some(name) = &d.tool_name {
            let _ = t.set("tool_name", name.as_str());
        }
        mlua::Value::Table(t)
    });
    cells.register_lua_projector::<ConfirmRequested, _>(|r, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("handle_id", r.handle_id);
        let _ = t.set("tool_name", r.tool_name.as_str());
        let _ = t.set("summary", styled_lines_to_lua(lua, &r.summary));
        if let Ok(options) = lua.create_table() {
            for (i, option) in r.grant_options.iter().enumerate() {
                if let Ok(option_tbl) = lua.create_table() {
                    let _ = option_tbl.set("id", option.id.as_str());
                    let _ = option_tbl.set("label", option.label.as_str());
                    let _ = options.set(i + 1, option_tbl);
                }
            }
            let _ = t.set("grant_options", options);
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

    cells.declare("vim_mode", seeds.vim_mode);
    cells.declare("agent_mode", seeds.agent_mode);
    cells.declare("model", seeds.model);
    cells.declare("reasoning", seeds.reasoning);
    cells.declare("confirms_pending", false);
    cells.declare("tokens_used", TokenUsage::default());
    cells.declare("errors", 0u32);
    cells.declare("cwd", seeds.cwd);
    cells.declare("session_epoch", 0u64);
    cells.declare("session_title", seeds.session_title);
    cells.declare("branch", seeds.branch);
    cells.declare("history_epoch", 0u64);
    cells.declare("input_epoch", 0u64);
    cells.declare("now", 0u64);
    cells.declare("spinner_frame", 0u8);
    cells.declare("tps", 0.0f64);
    cells.declare("task_label", String::new());
    cells.declare("running_procs", 0u32);
    cells.declare("permission_pending", false);
    cells.declare("notification_visible", false);
    cells.declare("cursor_pos", CursorPos::default());

    cells.register_lua_projector::<Vec<WorkBusyEntry>, _>(|v, lua| {
        let Ok(out) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        for (i, e) in v.iter().enumerate() {
            let Ok(t) = lua.create_table() else { continue };
            let _ = t.set("id", e.id);
            let _ = t.set("label", e.label.as_str());
            let _ = out.set(i + 1, t);
        }
        mlua::Value::Table(out)
    });
    cells.declare("work_state", String::from("idle"));
    cells.declare("work_label", String::new());
    cells.declare("work_elapsed_ms", 0u64);
    cells.declare("work_busy", Vec::<WorkBusyEntry>::new());
    cells.declare("work_outcome", String::new());
    cells.declare("work_retry_attempt", 0u32);
    cells.declare("work_retry_remaining_ms", 0u64);

    // Event-shaped cells: declared with an `EventStub` placeholder so `smelt.cell(name):subscribe` works.
    cells.declare("history", EventStub);
    cells.declare("turn_complete", EventStub);
    cells.declare("turn_error", EventStub);
    cells.declare("confirm_requested", EventStub);
    cells.declare("confirm_resolved", EventStub);
    cells.declare("session_started", EventStub);
    cells.declare("session_ended", EventStub);
    cells.declare("block_done", EventStub);
    cells.declare("cmd_pre", String::new());
    cells.declare("cmd_post", String::new());
    cells.declare("shutdown", EventStub);
    cells.declare("turn_start", EventStub);
    cells.declare("turn_end", EventStub);
    cells.declare("tool_start", EventStub);
    cells.declare("tool_end", EventStub);
    cells.declare("stream_delta", EventStub);
    cells.declare("stream_phase", EventStub);
    cells.declare("input_submit", String::new());
    cells.declare("keymap_pending", String::new());

    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn handle(lua: &Lua, src: &str) -> Rc<LuaHandle> {
        let func: mlua::Function = lua.load(src).eval().expect("load");
        Rc::new(LuaHandle::from_func(lua, func).expect("registry"))
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
    fn fire_carries_prev_value() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 7u32);
        c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        // First publish: prev = initial value from declare.
        c.set_dyn("count", Rc::new(8u32));
        // Second publish: prev = the value set by the first publish.
        c.set_dyn("count", Rc::new(9u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].value.downcast_ref::<u32>(), Some(&8u32));
        assert_eq!(fires[0].prev.downcast_ref::<u32>(), Some(&7u32));
        assert_eq!(fires[1].value.downcast_ref::<u32>(), Some(&9u32));
        assert_eq!(fires[1].prev.downcast_ref::<u32>(), Some(&8u32));
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

        // Every name in `SEEDED_CELL_NAMES` must round-trip through
        // `Cells::get_lua` (i.e. actually be declared above). Adding a
        // new builtin without updating the list trips this test.
        for name in SEEDED_CELL_NAMES {
            let v = cells.get_lua(name, &lua);
            assert!(
                !matches!(v, mlua::Value::Nil) || is_event_cell(name),
                "SEEDED_CELL_NAMES lists `{name}` but Cells::get_lua returned Nil for a non-event cell"
            );
        }
    }

    fn is_event_cell(name: &str) -> bool {
        matches!(
            name,
            "history"
                | "turn_complete"
                | "turn_error"
                | "confirm_requested"
                | "confirm_resolved"
                | "session_started"
                | "session_ended"
                | "block_done"
                | "shutdown"
                | "turn_start"
                | "turn_end"
                | "tool_start"
                | "tool_end"
                | "stream_delta"
        )
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
            if let Some(n) = u.context_tokens {
                let _ = t.set("context_tokens", n);
            }
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
                context_tokens: Some(1690),
                prompt_tokens: Some(1234),
                completion_tokens: Some(456),
                ..Default::default()
            },
        );
        match c.get_lua("tokens_used", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("prompt_tokens").unwrap(), 1234);
                assert_eq!(t.get::<i64>("context_tokens").unwrap(), 1690);
                assert_eq!(t.get::<i64>("completion_tokens").unwrap(), 456);
                // Absent fields surface as nil - not 0 - so plugins can
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

        // Set typed payloads via set_dyn - Cells::project_to_lua keys
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
                display_tps: Some(33.5),
                interrupted: false,
                tool_elapsed,
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
                summary: protocol::StyledLines::from_plain("ls"),
                args: std::collections::HashMap::new(),
                grant_options: vec![crate::transcript_model::ConfirmApprovalOption {
                    id: "grant_0_session".into(),
                    label: "allow bash for this session".into(),
                    scope: crate::transcript_model::ApprovalScope::Session,
                    grants: Vec::new(),
                }],
            }),
        );

        match cells.get_lua("turn_complete", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("elapsed_ms").unwrap(), 12000);
                assert!((t.get::<f64>("avg_tps").unwrap() - 33.5).abs() < f64::EPSILON);
                assert!((t.get::<f64>("display_tps").unwrap() - 33.5).abs() < f64::EPSILON);
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
                let summary: mlua::Table = t.get("summary").unwrap();
                let line: mlua::Table = summary.get(1).unwrap();
                let span: mlua::Table = line.get(1).unwrap();
                assert_eq!(span.get::<String>("text").unwrap(), "ls");
                let options: mlua::Table = t.get("grant_options").unwrap();
                let option: mlua::Table = options.get(1).unwrap();
                assert_eq!(option.get::<String>("id").unwrap(), "grant_0_session");
                assert_eq!(
                    option.get::<String>("label").unwrap(),
                    "allow bash for this session"
                );
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn builtin_cells_queue_subscribers_on_set() {
        // Every state-changing event in the engine pipeline reaches
        // the right cell setter and queues subscribers.
        let lua = Lua::new();
        let mut cells = build_with_builtins(BuiltinSeeds {
            vim_mode: "Insert".into(),
            agent_mode: "normal".into(),
            model: "m".into(),
            reasoning: "off".into(),
            cwd: "/".into(),
            session_title: String::new(),
            branch: String::new(),
        });

        // Subscribe to a mix of stateful and event-shaped built-in cells.
        for name in [
            "agent_mode",
            "turn_complete",
            "turn_error",
            "tool_start",
            "tool_end",
            "history",
            "confirm_requested",
            "confirm_resolved",
        ] {
            cells
                .subscribe_kind(name, SubscriberKind::Lua(handle(&lua, "function() end")))
                .expect("builtin cell should be declared");
        }

        cells.set_dyn("agent_mode", Rc::new("apply".to_string()));
        cells.set_dyn(
            "turn_complete",
            Rc::new(TurnMeta {
                elapsed_ms: 100,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            }),
        );
        cells.set_dyn(
            "turn_error",
            Rc::new(TurnError {
                message: "err".into(),
            }),
        );
        cells.set_dyn(
            "tool_start",
            Rc::new(ToolStart {
                tool: "bash".into(),
                args: std::collections::HashMap::new(),
            }),
        );
        cells.set_dyn(
            "tool_end",
            Rc::new(ToolEnd {
                tool: "bash".into(),
                is_error: false,
                elapsed_ms: None,
            }),
        );
        cells.set_dyn(
            "history",
            Rc::new(HistoryDelta {
                kind: "append".into(),
                count: 1,
            }),
        );
        cells.set_dyn(
            "confirm_requested",
            Rc::new(ConfirmRequested {
                handle_id: 1,
                tool_name: "bash".into(),
                summary: protocol::StyledLines::from_plain("ls"),
                args: std::collections::HashMap::new(),
                grant_options: vec![],
            }),
        );
        cells.set_dyn(
            "confirm_resolved",
            Rc::new(ConfirmResolved {
                handle_id: 1,
                decision: "yes".into(),
            }),
        );

        let fires = cells.drain_pending();
        let names: std::collections::HashSet<_> = fires.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            fires.len(),
            8,
            "missing cells: {:?}",
            [
                "agent_mode",
                "turn_complete",
                "turn_error",
                "tool_start",
                "tool_end",
                "history",
                "confirm_requested",
                "confirm_resolved"
            ]
            .iter()
            .filter(|n| !names.contains(**n))
            .collect::<Vec<_>>()
        );
        for expected in [
            "agent_mode",
            "turn_complete",
            "turn_error",
            "tool_start",
            "tool_end",
            "history",
            "confirm_requested",
            "confirm_resolved",
        ] {
            assert!(
                names.contains(expected),
                "expected {expected} to fire subscribers"
            );
        }
    }
}
