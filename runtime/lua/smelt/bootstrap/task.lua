-- Task-yielding primitives: `smelt.sleep` and `smelt.task` wait, external,
-- timeout, race and all. Autoloaded before user init.lua so all plugins see them.

local internal = __smelt_internal

-- Yieldable-context guard. `level = 3` so the error points at the caller's
-- caller (the user code), not this helper.
local function require_yieldable(name)
  if not coroutine.isyieldable() then
    error(name .. ": call from inside smelt.spawn(fn) or tool.execute", 3)
  end
end

internal.require_yieldable = require_yieldable

-- Yield with a payload and unwrap cancellation uniformly. Internal to the
-- task primitives below - single source for the `__cancelled` check.
local function yield_with_cancel(payload)
  local result = coroutine.yield(payload)
  if type(result) == "table" and result.__cancelled then
    error("cancelled", 0)
  end
  return result
end

-- Check whether an error value represents a task or engine-ask cancellation.
-- Matches both the task-runtime `RuntimeError("cancelled")` shape and the
-- engine-ask `{ kind = "cancelled", message = "..." }` shape.
---@type fun(err: any): boolean
function smelt.task.is_cancelled(err)
  if type(err) == "table" and err.kind == "cancelled" then
    return true
  end
  if type(err) == "string" then
    local first = err:match("^([^\r\n]*)")
    return first == "cancelled"
  end
  return false
end

-- Sleep for `ms` milliseconds. Must be called from inside `smelt.spawn(fn)`
-- or a `tool.execute`. Raises `cancelled` if the task is cancelled while
-- parked.
---@type fun(ms: integer): any
function smelt.sleep(ms)
  require_yieldable("smelt.sleep")
  return yield_with_cancel({ __yield = "sleep", ms = ms })
end

-- Park the running task until `smelt.task.resume(id, value)` fires. Returns the resumed value.
-- Pass `{ interactive = true }` (or `{ pauses_deadline = true }`) for user-facing
-- waits such as dialogs; tool watchdog deadlines do not count wall time spent
-- waiting for the user.
---@type fun(id: integer, opts?: table): any
function smelt.task.wait(id, opts)
  require_yieldable("smelt.task.wait")
  opts = opts or {}
  return yield_with_cancel({
    __yield = "external",
    id = id,
    pauses_deadline = opts.pauses_deadline or opts.interactive or false,
  })
end

-- Allocate an external task id, invoke `start(id)` to kick off whatever
-- will eventually call `smelt.task.resume(id, value)` (or resolve through
-- the Rust resume sink), and park until that resolution arrives. Returns
-- the resolved value. Raises `cancelled` if the task is cancelled while
-- parked. Plugin authors bridging custom Rust extensions use this to
-- avoid hand-rolling the alloc + start + wait dance.
---@type fun(start: fun(id: integer)): any
function smelt.task.external(start)
  require_yieldable("smelt.task.external")
  local id = smelt.task.alloc()
  start(id)
  return yield_with_cancel({ __yield = "external", id = id })
end

-- Run `fn` with an `ms`-millisecond deadline. Returns `(result, nil)` if
-- `fn` finishes in time, or `(nil, "timeout")` if the deadline fires
-- first - in which case `fn`'s coroutine is cancelled (any in-flight
-- `smelt.sleep` / `task.wait` raises `cancelled` and the coroutine
-- unwinds). Must run inside a yielding context.
---@type fun(ms: integer, fn: fun(): any): any, string?
function smelt.task.timeout(ms, fn)
  require_yieldable("smelt.task.timeout")
  local payload = smelt.task.external(function(id)
    local timer_reg, task_reg
    local settled = false
    task_reg = smelt.spawn(function()
      local ok, value = pcall(fn)
      if settled then return end
      settled = true
      if timer_reg then timer_reg:remove() end
      smelt.task.resume(id, ok and { result = value } or { error = tostring(value) })
    end)
    timer_reg = smelt.timer.set(ms, function()
      if settled then return end
      settled = true
      task_reg:remove()
      smelt.task.resume(id, { timeout = true })
    end)
  end)
  if payload.timeout then return nil, "timeout" end
  if payload.error then return nil, payload.error end
  return payload.result, nil
end

-- Run `fns` concurrently; first to return wins. Returns the winner's
-- `(index, result)` as multi-value, mirroring `task.timeout`'s
-- `(value, err)` shape. All other branches are cancelled. Errors from
-- any branch propagate (losers cancelled first). Must run inside a
-- yielding context.
---@type fun(...: fun(): any): integer, any
function smelt.task.race(...)
  require_yieldable("smelt.task.race")
  local fns = { ... }
  if #fns == 0 then error("smelt.task.race: requires at least one function", 2) end
  local payload = smelt.task.external(function(id)
    local regs = {}
    local settled = false
    for i, fn in ipairs(fns) do
      regs[i] = smelt.spawn(function()
        local ok, value = pcall(fn)
        if settled then return end
        settled = true
        for j, r in ipairs(regs) do
          if j ~= i and r then r:remove() end
        end
        smelt.task.resume(id, ok and { index = i, result = value } or { error = tostring(value) })
      end)
    end
  end)
  if payload.error then error(payload.error, 2) end
  return payload.index, payload.result
end

-- Run `fns` concurrently; wait for all to finish. Returns an array of
-- results in the same order as the input. Errors from any branch
-- propagate; the remaining branches still complete and their results
-- are discarded. Must run inside a yielding context.
---@type fun(...: fun(): any): any[]
function smelt.task.all(...)
  require_yieldable("smelt.task.all")
  local fns = { ... }
  if #fns == 0 then return {} end
  local results = {}
  local first_err
  local payload = smelt.task.external(function(id)
    local remaining = #fns
    for i, fn in ipairs(fns) do
      smelt.spawn(function()
        local ok, value = pcall(fn)
        if ok then
          results[i] = value
        else
          first_err = first_err or tostring(value)
        end
        remaining = remaining - 1
        if remaining == 0 then
          smelt.task.resume(id, { error = first_err })
        end
      end)
    end
  end)
  if payload.error then error(payload.error, 2) end
  return results
end
