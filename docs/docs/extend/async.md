---
title: "Async tasks and I/O in plugins"
description: "Wait, sleep, spawn processes, search and watch files from a smelt Lua plugin with coroutines while the terminal UI keeps rendering."
---

# Tasks and I/O

Anything that waits (`smelt.sleep`, `smelt.dialog.open`, `smelt.picker.open`,
`smelt.tools.call`, `smelt.task.wait`) must run in a coroutine. While it waits,
smelt keeps rendering and handling input.

## Where you can wait

1. **Inside `tool.execute`**: plugin tools already run in a coroutine.
2. **Inside `smelt.spawn(fn)`**: start one yourself, for example from a command
   handler or a timer.

```lua
smelt.cmd.register("ps", function()
  smelt.spawn(function()
    local result = smelt.dialog.open({ ... })
    if result.action == "approve" then ... end
  end)
end)
```

Calling `smelt.sleep` outside a coroutine raises immediately.

`smelt.spawn` returns a `Reg`; `:remove()` cancels the coroutine, and any pending
wait raises `cancelled`. To manage several subscriptions as one, combine them:

```lua
return smelt.reg.compose(
  smelt.keymap.set("n", "<leader>x", handler),
  smelt.fs.watch(path, on_change),
  smelt.timer.every(1000, tick)
)
```

`smelt.reg.new(fn)` wraps any cleanup function as a `Reg`. To tell cancellation
from a real error after `pcall`, use `smelt.task.is_cancelled(err)`:

```lua
local ok, value = pcall(function()
  return smelt.process.run("slow-command", {})
end)
if not ok and smelt.task.is_cancelled(value) then return end
if not ok then error(value, 0) end
```

## Errors

Invalid arguments and unavailable capabilities raise Lua errors. Fallible I/O
returns `(value, nil)` or `(nil, message)`. A process that exits non-zero is still
a normal result. Tool calls return `{ content, is_error, metadata? }`, because a
tool failure is something the model should see, not a Lua error.

## Timeouts and concurrency

`smelt.task.timeout`, `smelt.task.race` and `smelt.task.all` combine waits. They
must run in a coroutine.

```lua
-- Give up after a deadline.
local out, err = smelt.task.timeout(2000, function()
  return smelt.process.run("slow-command", {})
end)
if err == "timeout" then ... end

-- First to finish wins; the loser is cancelled.
local index, result = smelt.task.race(
  function() return smelt.fs.read_async("/etc/hostname") end,
  function() smelt.sleep(500); return "fallback" end
)

-- Wait for everything; results keep input order.
local results = smelt.task.all(
  function() return smelt.fs.read_async("a.txt") end,
  function() return smelt.fs.read_async("b.txt") end
)
```

## Files, processes and search

`smelt.process.run` and `smelt.grep.run` wait without blocking the UI and must run
in a coroutine. `smelt.fs.read_async` and `write_async` do the same for large
files, while `smelt.fs.read` and `write` are synchronous and fine for small
config-time reads.

```lua
smelt.spawn(function()
  local content, err = smelt.fs.read_async("/path/to/big.json")
  if not content then return io.stderr:write(err) end
  smelt.fs.write_async("/tmp/out", transform(content))

  local out = smelt.process.run("ripgrep", { "TODO", "." })
  if out then print(out.stdout) end

  local matches = smelt.grep.run("TODO", ".", { line_numbers = true })
  if matches then print(matches.stdout) end
end)
```

If the coroutine is cancelled, `process.run` stops the child process, `grep.run`
stops `rg`, and an in-flight `read_async` or `write_async` finishes and its result
is discarded.

## Watching files

`smelt.fs.watch(path, handler, opts?)` calls `handler(event)` for each change
under `path`, where `event = { kind, detail?, paths }`.

- `kind`: `"create"`, `"modify"`, `"remove"`, `"rename"`, `"access"`, `"other"`
  or `"any"`.
- `detail`: a finer kind when available, for example `"file"` or `"folder"` for a
  create, `"from"`, `"to"` or `"both"` for a rename, `"data"` or `"metadata"` for
  a modify.
- `paths`: the affected paths.

Pass `recursive = false` to watch only direct children. It returns a `Reg`.

```lua
local reg = smelt.fs.watch(smelt.session.info().cwd, function(ev)
  for _, p in ipairs(ev.paths) do
    smelt.log.info(ev.kind .. " " .. p)
  end
end)
-- later: reg:remove()
```
