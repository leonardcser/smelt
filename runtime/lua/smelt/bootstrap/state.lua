local internal = __smelt_internal

-- Per-name state. Two flavours:
--   smelt.state.get(name?)        → ephemeral table; survives /reload only.
--   smelt.state.persistent(name)  → JSON-backed wrapper; survives restart.
--
-- Ephemeral storage lives in the VM-private capability tree so bootstrap
-- re-runs preserve it without exposing it to user chunks. Touch tracking is
-- reset on every reload; Rust invokes the private sweep hook after autoload to
-- prune slots no plugin touched this cycle.
internal.__state = internal.__state or {}
internal.__persistent_state = internal.__persistent_state or {}
local state = internal.__state
local state_touched = {}
local persistent_state = internal.__persistent_state
local persistent_state_touched = {}

-- Persistent wrapper: backed by JSON under
-- `$XDG_STATE_HOME/smelt/plugins/<name>.json`. Top-level writes are
-- debounced and auto-saved; nested mutations require an explicit
-- `s.save()` call. Reads pass through to the loaded table.
local function persistent_entry(name)
  persistent_state_touched[name] = true
  local entry = persistent_state[name]
  if not entry then
    entry = { data = internal.state.__load(name), pending = nil, dirty = false }
    persistent_state[name] = entry
  end
  return entry
end

function __smelt_internal.__flush_persistent_state()
  local errors = {}
  for name, entry in pairs(persistent_state) do
    if entry.pending then entry.pending:remove(); entry.pending = nil end
    if entry.dirty then
      local ok, err = pcall(internal.state.__save, name, entry.data or {})
      if ok then
        entry.dirty = false
      else
        errors[#errors + 1] = tostring(err)
      end
    end
  end
  if #errors > 0 then error(table.concat(errors, "\n"), 2) end
end

-- Return the JSON-backed state wrapper for `name`. Top-level assignments are
-- saved after `opts.debounce_ms` (100 ms by default). Call `state.save()` after
-- mutating nested values; `state.all` exposes the underlying table. Pending
-- top-level writes are also flushed during runtime teardown and reload.
---@type fun(name: string, opts: { debounce_ms: integer? }?): table
smelt.state.persistent = function(name, opts)
  opts = opts or {}
  local debounce_ms = opts.debounce_ms or 100
  local entry = persistent_entry(name)
  local data = entry.data
  local function flush()
    if entry.pending then entry.pending:remove(); entry.pending = nil end
    internal.state.__save(name, data)
    entry.dirty = false
  end
  local function schedule()
    entry.dirty = true
    if entry.pending then return end
    entry.pending = smelt.timer.set(debounce_ms, function()
      entry.pending = nil
      internal.state.__save(name, data)
      entry.dirty = false
    end)
  end
  return setmetatable({}, {
    __index = function(_, k)
      if k == "save" then return flush end
      if k == "all" then return data end
      return data[k]
    end,
    __newindex = function(_, k, v)
      data[k] = v
      schedule()
    end,
    __pairs = function() return pairs(data) end,
  })
end

-- Plugin scope stack. Each Rust loader (autoload, init.lua, plugin
-- files) pushes a placeholder frame around the module body via the private
-- scope wrapper. The frame stays unnamed (`false`) by default -
-- the module body opts in to hot-reload survival by calling
-- `smelt.plugin("name")`, which promotes its frame to that name.
-- While the frame is named, `smelt.state.get()` and the unnamed-resource
-- constructors (`smelt.paint.register`, `smelt.overlay.new`,
-- `smelt.win.new`, `smelt.buf.new`) auto-name on the plugin's behalf
-- so survival is implicit for the rest of the body.
local scope_stack = {}
-- Per-scope per-type counter - minted in declaration order during a
-- module body run, reset every time `smelt.plugin(name)` promotes a
-- fresh frame so auto-names stay stable across `/reload` when the
-- module body runs the same constructors in the same order.
local scope_counters = {}

-- Push an unnamed frame, run `fn(...)`, pop it. The frame starts as
-- `false` (no auto-naming, no scoped state); the module body can
-- promote it to a named plugin scope via `smelt.plugin(name)`. Errors
-- propagate after restoring the stack so a failing module body doesn't
-- leak its frame.
function __smelt_internal.__with_scope(fn, ...)
  scope_stack[#scope_stack + 1] = false
  local ok, ret = pcall(fn, ...)
  scope_stack[#scope_stack] = nil
  if not ok then error(ret, 0) end
  return ret
end

-- Resolve the current scope name, if any. Returns nil when no plugin
-- scope is active (no `smelt.plugin(...)` call, or running inside a
-- callback fired from the event loop). Used by `smelt.state.get()` and
-- unnamed-resource auto-naming.
function __smelt_internal.__current_scope()
  local top = scope_stack[#scope_stack]
  if top == false then return nil end
  return top
end

-- Mint the next auto-name for `kind` ("paint" | "buf" | "win" | "overlay")
-- in the current scope. Returns nil if no scope is active. The naming
-- is `"<scope>:<kind>:<idx>"`, deterministic per (scope, kind, declaration
-- order); idx counts from 0.
function __smelt_internal.__auto_name(kind)
  local scope = internal.__current_scope()
  if not scope then return nil end
  local counters = scope_counters[scope]
  if not counters then
    counters = { paint = 0, buf = 0, win = 0, overlay = 0 }
    scope_counters[scope] = counters
  end
  local idx = counters[kind] or 0
  counters[kind] = idx + 1
  return string.format("%s:%s:%d", scope, kind, idx)
end

--- Handle returned by `smelt.plugin` for a named plugin scope.
---@class smelt.Plugin
---@field name string Stable scope name.
---@field state table Ephemeral JSON-shaped state that survives `/reload` but not restart.

-- Promote the current loader frame to plugin scope `name` and return a
-- small handle exposing the plugin's per-cycle state slot:
--
--   local M = smelt.plugin("banner")
--   M.state.fires = 0          -- M.state is smelt.state.get("banner")
--   M.name == "banner"
--
-- After this call, bare `smelt.state.get()` also resolves to the named
-- slot and unnamed resource constructors auto-name keyed by `name`.
-- Idempotent within a single module body run: counters reset on every
-- promotion so declaration order is what matters.
--
-- The handle deliberately doesn't wrap `smelt.signal` / `smelt.cmd` /
-- `smelt.keymap` / `smelt.lifecycle.*` - those calls would not be
-- scope-aware (signal/cmd names are global), so a method facade would
-- imply encapsulation it can't deliver. Call them directly through
-- `smelt.*` and namespace your signal/cmd names explicitly.
--
-- Must be called from a module body (or init.lua). Outside a loader
-- frame (e.g. from an event callback) it raises immediately.
---@type fun(name: string): smelt.Plugin
function smelt.plugin(name)
  if type(name) ~= "string" or name == "" then
    error("smelt.plugin: name must be a non-empty string", 2)
  end
  local i = #scope_stack
  if i == 0 then
    error("smelt.plugin: must be called from a module body (or init.lua)", 2)
  end
  scope_stack[i] = name
  scope_counters[name] = { paint = 0, buf = 0, win = 0, overlay = 0 }
  return setmetatable({ name = name }, {
    __index = function(_, key)
      if key == "state" then return smelt.state.get(name) end
    end,
  })
end

-- Return an ephemeral state table. With an explicit name, returns the
-- table for that name. With no arg, returns the current plugin's scoped
-- table keyed by the current scope name. Raises if called with no arg
-- outside a module body (no scope active).
---@type fun(name?: string): table
function smelt.state.get(name)
  if name == nil then
    name = internal.__current_scope()
    if not name then
      error("smelt.state.get(): no plugin scope active - pass an explicit name from outside module body", 2)
    end
  end
  state_touched[name] = true
  local s = state[name]
  if not s then
    s = {}
    state[name] = s
  end
  return s
end

function __smelt_internal.__sweep_state()
  for k in pairs(state) do
    if not state_touched[k] then
      state[k] = nil
    end
  end
  for k, entry in pairs(persistent_state) do
    if not persistent_state_touched[k] and not entry.dirty then
      persistent_state[k] = nil
    end
  end
end
