local internal = __smelt_internal

smelt.tick = smelt.tick or {}

-- Reload-safe periodic work. Fires `fn` once right away and then every `secs`
-- seconds, idle or not. `fn` runs inside a fresh `smelt.spawn`, so it may
-- yield (HTTP, processes, sleeps). Safe to call from a plugin's module body:
-- `/reload` cancels the timers and the body re-arms them.
--
-- Returns a `Reg` whose `:remove()` stops it.
---@type fun(secs: number, fn: fun()): smelt.Reg
function smelt.tick.every(secs, fn)
  if type(secs) ~= "number" or secs <= 0 then
    error("smelt.tick.every: secs must be a positive number", 2)
  end
  if type(fn) ~= "function" then
    error("smelt.tick.every: fn must be a function", 2)
  end
  local function run() smelt.spawn(function() fn() end) end
  local first = smelt.timer.set(0, run)
  local every = smelt.timer.every(math.max(1, math.floor(secs * 1000)), run)
  return smelt.reg.new(function()
    first:remove()
    every:remove()
  end)
end

-- Deliver each event batch of watcher `watcher_id` to `on_events` until the
-- watcher stops. Handler errors are logged under `label`.
local function pump(watcher_id, label, on_events)
  return smelt.spawn(function()
    while true do
      local task_id = smelt.task.alloc()
      internal.fs.__watch_arm(watcher_id, task_id)
      local events = smelt.task.wait(task_id)
      if events == nil then return end
      local ok, err = pcall(on_events, events)
      if not ok then
        smelt.log.error(label .. "_handler_failed", { error = tostring(err) })
      end
    end
  end)
end

-- Filesystem watcher. Calls `handler(event)` for each event, where
-- `event = { kind, detail?, paths }`. `kind` is one of `"create" | "modify" | "remove" | "rename" | "access" | "other" | "any"`;
-- `detail` carries notify's sub-kind when one is reported (e.g. `kind = "create"` → `detail = "file" | "folder"`).
-- `opts.recursive` defaults to true; set false to watch only the immediate
-- entries of a directory. Returns a `Reg` whose `:remove()` stops the
-- watcher and cancels the polling coroutine.
---@type fun(path: string, handler: fun(event: { kind: string, detail: string?, paths: string[] }), opts: table?): smelt.Reg
function smelt.fs.watch(path, handler, opts)
  if type(path) ~= "string" then
    error("smelt.fs.watch: path must be a string", 2)
  end
  if type(handler) ~= "function" then
    error("smelt.fs.watch: handler must be a function", 2)
  end
  local watcher_id, err = internal.fs.__watch_register(path, opts or {})
  if not watcher_id then
    error("smelt.fs.watch: " .. tostring(err), 2)
  end
  local task = pump(watcher_id, "fs.watch", function(events)
    for _, ev in ipairs(events) do
      local ok, perr = pcall(handler, ev)
      if not ok then
        smelt.log.error("fs.watch_handler_failed", {
          path = path,
          event = ev,
          error = tostring(perr),
        })
      end
    end
  end)
  return smelt.reg.compose(task, smelt.reg.new(function()
    internal.fs.__watch_stop(watcher_id)
  end))
end

-- Watch a Git repository for changes that can alter `smelt.git.diff`, including
-- tracked files under ignored directories, the index, HEAD and refs. Reads,
-- untracked ignored files and lock files are dropped. Change bursts are
-- coalesced off-thread over 150 ms; `handler()` is an invalidation notice, not
-- a file-event stream. `opts.cwd` defaults to the runtime workspace. Registration
-- returns after watches are installed, or raises on setup failure. Returns a
-- `Reg` whose `:remove()` stops watching.
---@type fun(handler: fun(), opts: { cwd: string? }?): smelt.Reg
function smelt.git.watch(handler, opts)
  if type(handler) ~= "function" then
    error("smelt.git.watch: handler must be a function", 2)
  end
  opts = opts or {}
  local watcher_id, err = internal.git.__watch_register(opts)
  if not watcher_id then
    error("smelt.git.watch: " .. tostring(err), 2)
  end
  local task = pump(watcher_id, "git.watch", function() handler() end)
  return smelt.reg.compose(task, smelt.reg.new(function()
    internal.fs.__watch_stop(watcher_id)
  end))
end
