---
title: "Write smelt plugins in Lua"
description: "Extend the smelt coding agent with plain Lua files: slash commands, events, tools, dialogs, overlays and transcript rendering, with hot reload and no build step."
---

# Extend

smelt exposes a Lua API on the global `smelt` table. With it you can register
slash commands, react to events, add tools the agent can call, build dialogs and
overlays, and change how the transcript looks. Extensions are plain Lua files:
no manifest, no build step.

!!! warning "Plugins are trusted code"

    Lua config and plugins run in-process with your user's privileges and are
    not sandboxed. Review third-party plugins and project `.smelt/` config before
    you trust them.

!!! tip "Just ask the agent"

    The built-in `customize` skill knows the Lua API and your config layout, so
    the agent can write, install and reload a plugin for you. Describe what you
    want, then treat the result as a starting point.

The bundled plugins in
[`runtime/lua/smelt/plugins/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/plugins)
are the best examples; every pattern in these guides comes from them.
For small plugins to copy, recorded in action, see the
[plugin gallery](gallery.md).

## Anatomy of a plugin

A plugin is a Lua module. Top-level statements run when it loads, so that is
where you register behavior.

```lua
-- ~/.config/smelt/plugins/hello.lua
local M = smelt.plugin("hello")

M.state.greetings = M.state.greetings or 0
smelt.cmd.register("hello", function(arg)
  M.state.greetings = M.state.greetings + 1
  local name = (arg and arg ~= "") and arg or "world"
  smelt.notify.info("hello, " .. name)
end, { desc = "greet someone" })

return M
```

Save it and `/hello` is available straight away:

<figure class="term" data-replay="../../assets/replays/plugin.json" aria-label="Running the /hello command from the plugin" markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>

`smelt.plugin(name)` gives the module a stable identity. It exposes `M.state`,
which survives `/reload`, and it names the buffers, windows and overlays you
create so they reconnect after a reload. It does not prefix command, keymap or
event names, so pick distinctive ones.

Files in `~/.config/smelt/plugins/` and `.smelt/plugins/` load automatically.
Reusable modules under `~/.config/smelt/lua/` load with `require`:

```lua
require("smelt.plugins.which_key") -- bundled opt-in plugin
require("hello")                   -- ~/.config/smelt/lua/hello.lua
```

## Editor setup

smelt mirrors its Lua runtime, including type stubs for every API, to
`~/.local/share/smelt/builtins/lua/` (`%LOCALAPPDATA%\smelt\data\builtins\lua\`
on Windows). Point Lua Language Server at it for completions and diagnostics.
Treat the folder as read-only; upgrades replace it.

```json
{
  "runtime.version": "Lua 5.4",
  "workspace.library": ["/home/you/.local/share/smelt/builtins/lua"],
  "diagnostics.globals": ["smelt"]
}
```

In a source checkout, use `runtime/lua` as the library path.

## Host and UiHost

Every API belongs to one of two tiers, shown on its reference page.

- **Host** works everywhere, including `smelt --headless`: `smelt.fs`,
  `smelt.http`, `smelt.process`, `smelt.events`, `smelt.tools`, and so on.
- **UiHost** needs the terminal UI and raises in headless mode: `smelt.win`,
  `smelt.buf`, `smelt.dialog`, `smelt.overlay`, `smelt.prompt`, and so on. A few
  modules, such as `smelt.theme` and `smelt.notify`, mix both: each reference
  entry states which tier it needs.

Keep core logic on Host APIs and presentation on UiHost APIs, and the same plugin
works in the TUI and in CI.

## Hot reload

Saved Lua files reload automatically; `F5` or `/reload` reloads manually. The
transcript and agent state stay put, and a failing file leaves the previous
config running (see [Reloading](../customize/index.md#reloading)).

To keep UI state across reloads:

1. Start the module with `smelt.plugin(name)`.
2. Keep rebuildable state, such as whether a panel is open, in `M.state`. It
   survives `/reload` but not a restart.
3. Re-open the UI from the module body when that state says it was open. Named
   buffers, windows and overlays reconnect instead of duplicating.

```lua
local M = smelt.plugin("my_plugin")

local function open()
  local buf = smelt.buf.new()
  local win = smelt.win.new(buf, { surface = "readonly_text" })
  smelt.overlay.new({ layout = smelt.ui.layout.leaf(win) })
  M.state.is_open = true
end

if M.state.is_open then open() end
return M
```

Pass an explicit `opts.name` when a resource must keep its identity across
reordering or when you create several dynamically. Without `smelt.plugin`,
resources are anonymous and are removed on reload.

Use `smelt.lifecycle.on_ready(fn)` for code that must run after all plugins have
loaded. It receives `ctx.kind` (`"launch"` or `"reload"`) so launch-only work can
skip reloads.

## Plugin state

`smelt.state.get(name)` returns a table that survives `/reload` but not a
restart. Use it for UI state and caches.

```lua
local s = smelt.state.get("my_plugin")
s.counter = (s.counter or 0) + 1
```

`smelt.state.persistent(name)` is backed by a JSON file in the state directory
(see [Storage and environment](../reference/storage.md)) and survives restarts.
Top-level assignments save automatically; after changing nested values call
`.save()`.

```lua
local s = smelt.state.persistent("recent_files")
s.last_opened = "/path/to/file"   -- saved automatically
s.history = s.history or {}
table.insert(s.history, "another")
s.save()                          -- nested change: save explicitly
```

## String-literal aliases

Parameters typed `smelt.<namespace>.<Name>` accept a closed set of strings, and
your editor autocompletes and checks them. Use the canonical name:
`smelt.vim.set_mode("normal")` works, `"n"` does not. Open aliases such as
[`smelt.signal.Name`](../reference/api/types.md#smeltsignalname) accept any string
and list well-known names as hints.
