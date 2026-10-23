local internal = __smelt_internal

smelt.tick = smelt.tick or {}

-- Reload-safe periodic work. Subscribes to the `now` cell (a one-second
-- wall-clock tick the host publishes while the app is alive) and fires
-- `fn` at most once every `secs` seconds. `fn` runs inside a fresh
-- `smelt.spawn`, so it may yield (HTTP, processes, sleeps) without
-- blocking the cell pump.
--
-- Unlike `smelt.timer.every`, this idiom is safe to call from a
-- plugin's module body: cell subscriptions are wiped on `/reload` and
-- re-armed when the body re-runs, so no Reg juggling is required for
-- hot-reload survival.
--
-- Returns a `Reg` whose `:remove()` unsubscribes.
---@type fun(secs: integer, fn: fun()): smelt.Reg
function smelt.tick.every(secs, fn)
  if type(secs) ~= "number" or secs <= 0 then
    error("smelt.tick.every: secs must be a positive number", 2)
  end
  if type(fn) ~= "function" then
    error("smelt.tick.every: fn must be a function", 2)
  end
  local last = 0
  return smelt.signal.subscribe("now", function(now)
    if (now or 0) - last >= secs then
      last = now or 0
      smelt.spawn(function() fn() end)
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
  local task = smelt.spawn(function()
    while true do
      local task_id = smelt.task.alloc()
      internal.fs.__watch_arm(watcher_id, task_id)
      local events = smelt.task.wait(task_id)
      if events == nil then return end
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
    end
  end)
  return smelt.reg.compose(task, smelt.reg.new(function()
    internal.fs.__watch_stop(watcher_id)
  end))
end
