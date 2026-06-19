# Plugin Authoring

smelt's runtime exposes a Lua API on the global `smelt` table. Register slash
commands, react to lifecycle events, paint extmarks, drive the engine, add
custom tools, and so on. Plugins are plain Lua files loaded from
`~/.config/smelt/plugins/*.lua`, `.smelt/plugins/*.lua`, or explicitly with
`require(...)` from `init.lua`.

The full surface lives in the [Lua API reference](../reference/api/index.md);
this page walks through the workflow and patterns for writing plugins against
it.

## Anatomy of a plugin

A plugin is just a Lua module. There is no manifest, no register-yourself call,
and no init hook. Top-level statements run when the file is `require`d, so
that's where you wire your behaviour up.

```lua
-- ~/.config/smelt/plugins/hello.lua
smelt.cmd.register("hello", function(arg)
  smelt.notify("hello, " .. ((arg and arg ~= "") and arg or "world"))
end, { desc = "greet someone" })

return {}
```

Load it automatically by leaving it under `~/.config/smelt/plugins/`, or load a
module explicitly from `init.lua`:

```lua
require("smelt.plugins.which_key") -- bundled opt-in plugin
require("hello")                  -- yours, if it is on package.path
```

For a real walkthrough, the bundled plugins under
[`runtime/lua/smelt/plugins/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/plugins)
are the canonical examples. Every pattern below comes straight from them.

## Hot reload

Edit any Lua file, then press `F5` or run `/reload`. Your config re-runs from
scratch; the current transcript and agent state stay put. Errors land in
`/messages`. Changes to [`early.lua`](customization.md#early-phase-config) need
a real restart.

Reload also refreshes the on-disk inputs that feed the agent's system prompt and
tool surface, not just Lua:

- `AGENTS.md` (global `~/.config/smelt/AGENTS.md` plus the nearest project copy)
  is re-read.
- Every `SKILL.md` under `~/.config/smelt/skills/`, `~/.claude/skills/`,
  `~/.agents/skills/`, `.smelt/skills/`, `.claude/skills/`, and
  `.agents/skills/` is rescanned, so new skills, renamed skills, and edits to
  descriptions or bodies all show up on the next turn.
- MCP servers declared with `smelt.mcp.register` are reconciled: new
  registrations spawn, removed registrations stop, and servers whose config
  changed are restarted. Pending tool calls finish on the old connection.
- `--system-prompt <file>`, when the flag pointed at a file path, is re-read
  from disk.

`smelt.settings.auto_reload` defaults to `true`, so Lua config edits usually
skip the manual F5: smelt watches Lua files under `~/.config/smelt/` and
`.smelt/`, debounces a 250 ms window, then runs `/reload` for you. Edits that
land while an agent turn is running or a modal dialog is open are deferred to
the next quiet window. Prompt inputs such as `AGENTS.md`, `SKILL.md`,
`--system-prompt` files, and markdown custom-command registration still require
manual `/reload`. Existing markdown custom commands read their file body on each
invocation, so content edits do not need reload unless you add, remove, or
rename the command file.

### Surviving reload smoothly

Module bodies run with the host pointer live on every Lua-context bring-up, both
cold start and `/reload`. Three pieces compose into "my UI keeps the same
position / focus / content when the user reloads my plugin":

1. **`smelt.state(name)`** - JSON-shaped table that survives `/reload` (not
   restart). Persist your `is_open` / cursor / variant index here.
2. **`opts.name = "..."`** on `smelt.overlay.new`, `smelt.win.new`,
   `smelt.buf.new`, and `smelt.paint.register` - opts the resource into
   hot-reload survival. The Rust-side structure stays in place; re-passing the
   same name on re-open swaps the layout / closure / contents atomically.
   Anonymous (no-name) resources get reaped each reload.
3. **Module-body re-open** - at the bottom of your file, check the state flag
   and re-call your `open()`. On cold start `is_open` is false, so it's a no-op;
   after `/reload` it's true, so `open()` re-runs and finds the named overlay /
   paint slot already there, just updating closures.

```lua
local function open()
  if STATE then return end
  STATE = {}
  STATE.buf     = smelt.buf.new   ({ name = "myplugin.buf" })
  STATE.win     = smelt.win.new   (STATE.buf, { name = "myplugin.win" })
  STATE.paint   = smelt.paint.register(paint_fn, { name = "myplugin.paint" })
  STATE.overlay = smelt.overlay.new({ name = "myplugin", layout = ... })
  persist().is_open = true
end

-- module body - runs on every Lua-context bring-up.
if persist().is_open then open() end
```

Paint leaves can also receive pointer events directly (`press`, `release`,
`drag`), which is useful for canvas-like overlays that should not be forced
through a buffer-backed window. The leaf that owned the `press` keeps receiving
`drag` and `release` even if the pointer drifts outside its rect:

```lua
local paint = smelt.paint.register(draw_fn, { name = "myplugin.paint" })
paint:on("press",   function(ev) smelt.notify("down @ "..ev.row..","..ev.col) end)
paint:on("drag",    function(ev) ... end)
paint:on("release", function(ev) ... end)
```

Use `smelt.lifecycle.on_ready(fn)` only when you need code that fires _after_
every bring-up's plugin pass completes (cell subscriptions, deferred wiring).
The hook fires with `ctx = { kind = "launch" | "reload" }` so launch-only
handlers can early-return on `ctx.kind ~= "launch"`.

## Bundled plugins

| Plugin         | Autoloaded | What it does                                                                                                                                                                                        |
| -------------- | ---------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `compact`      | yes        | Owns `/compact` and post-turn auto-compaction (uses inherited-session compaction retries and drives the prompt top-bar working indicator via `smelt.work.busy`)                                     |
| `esc_chord`    | yes        | `<Esc><Esc>` to cancel active work or rewind a turn                                                                                                                                                 |
| `debug_panel`  | yes        | F3 overlay with resolved model config, context tokens, and pricing                                                                                                                                  |
| `inspect`      | **opt-in** | Owns `/inspect`, starting the local session/request inspector web UI and opening it in a browser when available                                                                                     |
| `perf_panel`   | yes        | F12 overlay with live duration percentiles                                                                                                                                                          |
| `predict`      | yes        | After each turn, predicts your next message and shows it as ghost text                                                                                                                              |
| `scroll_pills` | yes        | While the transcript is scrolled away from the tail, shows two click-only overlays: a "↓ jump to bottom" pill above the prompt and a one-row "jump to next message" pill at the top of the terminal |
| `title`        | yes        | After each turn, generates a session title + slug if one isn't set                                                                                                                                  |
| `plan_mode`    | yes        | Registers the built-in `plan` mode after Normal, adds `present_plan`, and injects the plan-mode system prompt                                                                                       |
| `which_key`    | **opt-in** | Which-key style popup for pending global Lua keymaps                                                                                                                                                |

To enable an opt-in plugin, `require` it from `~/.config/smelt/init.lua`:

```lua
require("smelt.plugins.which_key")
require("smelt.plugins.inspect")
```

Autoloaded plugins can be disabled from `early.lua` with
`smelt.builtins.disable({ plugins = { "plan_mode" } })` when you explicitly want
to remove their built-in behavior.

## Host vs UiHost

Bindings are tagged with one of two tiers. The
[Lua API index](../reference/api/index.md) groups namespaces by tier and each
per-namespace page calls it out in the header.

- **Host**: works everywhere, including headless mode (`smelt --headless`).
  Examples: `smelt.fs`, `smelt.http`, `smelt.process`, `smelt.signal`,
  `smelt.events`, `smelt.tools`.
- **UiHost**: requires a live terminal UI. Calling a UiHost function from
  headless mode raises. Examples: `smelt.win`, `smelt.buf`, `smelt.theme`,
  `smelt.notify`, `smelt.statusline`, `smelt.keymap`.

The split matters because the same plugin can run in a TUI session and in a CI
script (`smelt --headless`). Keeping UI logic behind a tier check lets you write
one plugin that works everywhere: core logic in Host, presentation layer in
UiHost.

## Slash commands

`smelt.cmd.register` adds a `/name` command. The handler receives the argument
string (everything after `/name `, possibly empty); `opts` covers description,
busy-state policy, and visibility.

```lua
smelt.cmd.register("ps", function(_arg)
  -- ...
end, {
  desc            = "manage background processes",
  while_busy      = true,   -- callable while an agent turn is running (default)
  queue_when_busy = false,  -- reject vs queue when busy
  hidden          = false,  -- skip /help and the picker
})
```

Use `smelt.cmd.run("name args")` to invoke another command (with or without the
leading slash). Markdown files in `~/.config/smelt/commands/` register
automatically; see [Custom Commands](customization.md#custom-commands) for that
path.

## Lifecycle events

`smelt.events.on(name, handler)` subscribes to runtime events: agent turns,
session load, tool start/end, and so on. Events carry only future occurrences,
so handlers receive the payload without a previous value. `smelt.signal(name)`
subscribes to durable runtime state such as mode changes. Both return a `Reg`
whose `:remove()` drops the subscription. The full lists are the
`smelt.events.Name` and `smelt.signal.Name` aliases in
[`_types.lua`](https://github.com/leonardcser/smelt/blob/main/runtime/lua/smelt/_meta/_types.lua);
common ones:

| Name              | Payload                                   | When                         |
| ----------------- | ----------------------------------------- | ---------------------------- |
| `session_started` | none                                      | A session has been loaded    |
| `turn_start`      | none                                      | The agent dispatched a turn  |
| `turn_end`        | `{ cancelled }`                           | Turn complete or interrupted |
| `tool_start`      | `{ tool, args }`                          | A tool call began            |
| `tool_end`        | `{ tool, is_error, elapsed_ms }`          | A tool call finished         |
| `agent_mode`      | `"normal"`, `"plan"`, `"apply"`, `"yolo"` | Agent mode changed           |
| `input_submit`    | submitted text                            | User submitted a message     |
| `shutdown`        | none                                      | App is about to quit         |

```lua
smelt.events.on("turn_end", function(payload)
  if payload.cancelled then return end
  -- ... e.g. kick off a prediction call
end)

smelt.signal("agent_mode"):subscribe(function(mode)
  if mode == "plan" then activate() else deactivate() end
end)
```

You can declare your own durable signals with
`smelt.signal.new("my_plugin:state", initial)` and broadcast updates with
`smelt.signal("my_plugin:state"):set(value)`. For occurrence-shaped plugin
hooks, use `smelt.events.on("my_plugin:event", handler)` and
`smelt.events.emit("my_plugin:event", payload)`.

## Provider middleware

Use provider middleware when a plugin needs to observe or rewrite assembled
assistant responses:

```lua
smelt.provider.middleware({
  on_response = function(message)
    -- inspect or return a replacement assistant message
  end,
})
```

Hooks fire in registration order; each hook sees the previous hook's
replacement. To observe streaming tokens without mutating the response,
subscribe to runtime events such as `stream_delta`. See the
[`smelt.provider` reference](../reference/api/provider.md) for exact payload
shapes.

## Keymaps

`smelt.keymap.set(mode, chord, handler)`. The mode is `"n"`, `"i"`, `"v"`, or
`""` for any mode; handlers receive a context table.

```lua
smelt.keymap.set("n", "<C-y>", function()
  local where = smelt.focus()
  local text  = where == "transcript" and smelt.transcript.text() or smelt.prompt.text()
  smelt.clipboard.write(text)
end)

smelt.keymap.set("", "<Esc><Esc>", function(ctx)
  if ctx.vim_mode_at_chord_start == "insert" then
    -- ...
  end
  -- returning `false` lets the chord fall through to the next binding
end)
```

Per-window bindings (transcript-only, picker-only, etc.) go through
`win:key(chord, handler)`, which returns a `Reg` whose `:remove()` undoes the
binding.

## Window events and marks

Plugins that paint into existing buffers subscribe to per-window events and draw
with marks scoped to a namespace they own. The pattern is:

```lua
local prompt = smelt.prompt.win()
local ns     = smelt.ns("my_plugin")

prompt:on("text_changed", function()
  local buf = prompt:buf()
  if not buf then return end
  buf:clear_ns(ns):mark(ns, 1, 0, {
    end_col  = 999,
    hl_group = "DiagnosticHint",
    priority = 200,
  })
end)
```

Both `"text_changed"` and the `mark` opts table are type-checked: an unknown
event name or a typo'd field surfaces as a diagnostic in your editor before the
plugin ever runs.

## Floating windows and overlays

For overlays that own their own buffer and rect, such as picker panels, perf
HUDs, and side docks, open a buffer, attach it to a window, then mount that
window in an overlay:

```lua
local buf = smelt.buf.new()
local win = smelt.win.new(buf, { focusable = false })

smelt.overlay.new({
  title     = { { text = " perf ", bold = true } },
  anchor    = "screen_at",
  corner    = "ne",
  width     = 44,           -- cells
  height    = 14,           -- cells
  modal     = false,
  draggable = true,
  layout    = smelt.ui.layout.leaf(win),
})
```

Overlay sizing is two orthogonal concepts:

- **Anchor**: where the overlay lives. Valid values:
  - `"dock_bottom"` (default), docked above the statusline.
  - `"dock_top"` / `"dock_left"` / `"dock_right"`, docked to the named edge. All
    dock anchors reserve the bottom statusline row.
  - `"center"`, centered on the screen.
  - `"screen_at"`, absolute position; pair with `corner` + `row` + `col`.
  - `"win"`, attached to another window; pair with `target` (win id), `attach`
    (corner), and `row_offset` / `col_offset`.
- **Size**: `width` / `height` set a fixed extent; `max_width` / `max_height`
  shrink-to-fit with a cap. Setting both fixed and max on the same axis is an
  error. Each value accepts:
  - an integer (cells),
  - a `"N%"` string (percent of the anchor's available extent on that axis),
  - `"fill"` (the entire available extent).

Anchor defaults: `dock_bottom` / `dock_top` are full-width × 60% tall;
`dock_left` / `dock_right` are 30% wide × full-height; `center` is 70% × 60%;
`screen_at` / `win` default to 60×20 cells.

For modal dialogs (a markdown panel + an option list + a free-text input, etc.)
`smelt.dialog.open` is the higher-level surface. It returns the result of the
user's choice. The bundled dialogs in
[`runtime/lua/smelt/dialogs/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/dialogs)
(`confirm`, `permissions`, `resume`, `rewind`) are the reference
implementations.

Dialog height has two modes:

- `height = "N%"` (or cells, or `"fill"`): fixed size. Default `"60%"`. Use this
  when the body should always fill the dock regardless of content size.
- `max_height = "N%"` (or cells, or `"fill"`): dialog shrinks to fit its
  content, capped at this value. Panels with no explicit `height` default to
  `"fit"` so a single-panel dialog actually shrinks; longer content triggers the
  panel's scrollbar at the cap. Setting both `height` and `max_height` is an
  error.

## Tasks: tool calls, dialogs, sleeps

Anything that yields (`smelt.sleep`, `smelt.dialog.open`, `smelt.picker.open`,
`smelt.tools.call`, `smelt.task.wait`) must run inside a task-yielding context.
Yielding keeps the TUI responsive: while your coroutine waits for a dialog
answer or a slow HTTP response, the main thread continues rendering and handling
input. There are two contexts:

1. **Inside `tool.execute`**: every plugin tool already runs on a coroutine.
2. **Wrapped in `smelt.spawn(fn)`**: fire-and-forget coroutine for everything
   else (a slash-command handler that opens a dialog, a timer callback that
   parks on a tool call, etc.).

```lua
smelt.cmd.register("ps", function()
  smelt.spawn(function()
    local result = smelt.dialog.open({ ... })
    if result.action == "approve" then ... end
  end)
end)
```

Calling `smelt.sleep` from outside a yielding context raises immediately; that's
how you tell which side of the line you're on.

`smelt.spawn(fn)` returns a `Reg` whose `:remove()` cancels the coroutine. Any
in-flight `smelt.sleep` / `smelt.task.wait` raises `cancelled` and the task
unwinds. When a plugin owns several reactive subscriptions, combine them with
`smelt.reg.compose(...)` and return one handle:

```lua
return smelt.reg.compose(
  smelt.win.cur():key("n", "<leader>x", handler),
  smelt.fs.watch(path, on_change),
  smelt.timer.every(1000, tick)
)
```

`smelt.reg.new(fn)` wraps an arbitrary teardown function as a `Reg` for cases
that need custom cleanup logic.

## Concurrency combinators

`smelt.task.timeout`, `smelt.task.race`, and `smelt.task.all` compose multiple
coroutines through `smelt.spawn` + `smelt.task.external`. All require a yielding
context.

```lua
-- Bound a yielding op with a deadline.
local out, err = smelt.task.timeout(2000, function()
  return smelt.process.run("slow-command", {})
end)
if err == "timeout" then ... end

-- First to finish wins; losers are cancelled.
local winner = smelt.task.race(
  function() return smelt.fs.read_async("/etc/hostname") end,
  function() smelt.sleep(500); return "fallback" end
)
print(winner.index, winner.result)

-- Wait for everything; results stay in input order.
local results = smelt.task.all(
  function() return smelt.fs.read_async("a.txt") end,
  function() return smelt.fs.read_async("b.txt") end
)
```

## Plugin state

`smelt.state(name)` returns an ephemeral table scoped to `name`. Survives
`/reload` but not a restart. Use it for live UI state, such as whether a panel
is open, the current scroll position, or a cache that can be rebuilt. Plugins
removed since the last load have their slots swept automatically.

```lua
local s = smelt.state("my_plugin")
s.counter = (s.counter or 0) + 1
```

`smelt.state.persistent(name)` returns a JSON-backed wrapper that writes through
to `$XDG_STATE_HOME/smelt/plugins/<name>.json`. Use it for user preferences or
data that must survive restarts. Top-level assignments are debounced and
auto-saved; nested mutations need an explicit `:save()`.

```lua
local s = smelt.state.persistent("recent_files")
s.last_opened = "/path/to/file"   -- debounced auto-save
table.insert(s.history or {}, "another"); s.save()   -- nested → manual save
```

## Filesystem watching

`smelt.fs.watch(path, handler, opts?)` calls `handler(event)` for each
filesystem change under `path`. `event = { kind, detail?, paths }`:

- `kind`:
  `"create" | "modify" | "remove" | "rename" | "access" | "other" | "any"`.
- `detail`: finer-grained sub-kind when notify reports one. Examples:
  `kind = "create"` → `detail = "file" | "folder"`; `kind = "rename"` →
  `detail = "from" | "to" | "both"`; `kind = "modify"` →
  `detail = "data" | "metadata"`.
- `paths`: list of affected paths.

Set `opts.recursive = false` to watch only direct children. Returns a `Reg`:

```lua
local reg = smelt.fs.watch(vim.fn.getcwd(), function(ev)
  for _, p in ipairs(ev.paths) do
    smelt.log.info(ev.kind .. " " .. p)
  end
end)
-- later: reg:remove()
```

## Off-thread filesystem, process, and grep I/O

`smelt.process.run` and `smelt.grep.run` yield the calling coroutine through the
task runtime instead of blocking the main loop; they must run inside
`smelt.spawn(fn)` or a `tool.execute` body. Off-thread I/O keeps the TUI
responsive while a large file is read or a long command runs. The agent can
still stream tokens, and you can still scroll and type. The same is true for the
explicit `smelt.fs.read_async` / `smelt.fs.write_async` variants when reading or
writing large files. `smelt.fs.read` / `smelt.fs.write` stay synchronous and are
fine for small config-time reads:

```lua
smelt.spawn(function()
  local content, err = smelt.fs.read_async("/path/to/big.json")
  if not content then return io.stderr:write(err) end
  local ok = smelt.fs.write_async("/tmp/out", transform(content))

  local out = smelt.process.run("ripgrep", { "TODO", "." })
  if out then print(out.stdout) end

  local matches = smelt.grep.run("TODO", ".", { line_numbers = true })
  if matches then print(matches.stdout) end
end)
```

**Cancellation semantics.** When the calling coroutine is cancelled
(`smelt.task.timeout` deadline, `smelt.task.race` loser, or `:remove()` on the
spawn Reg), every yielding API raises `cancelled` and unwinds. The underlying
work differs by kind:

- `smelt.process.run`: the child's process group receives SIGTERM; the future
  resolves once the kill completes.
- `smelt.grep.run`: the `rg` child receives SIGKILL and the future resolves once
  `wait()` returns.
- `smelt.fs.read_async` / `smelt.fs.write_async`: the std::fs call can't be
  interrupted mid-syscall, so the worker thread runs to completion and the
  result is discarded. Bounded waste (file-size dependent); no external side
  effects leak.
- `smelt.sleep` / `smelt.task.wait`: instantaneous.

## Pickers

`smelt.picker.fuzzy(opts)` is the high-level entry point for choose-one prompts.
Items can be plain strings or `{ label, description, ansi_color, search_terms }`
records; ranking is delegated to `smelt.fuzzy.rank`. Returns
`{ index, item, action }` on accept or `nil` on dismiss.

```lua
smelt.spawn(function()
  local choice = smelt.picker.fuzzy({
    items = { "first", "second", "third" },
    placeholder = "pick one",
  })
  if choice then smelt.log.info("picked " .. choice.item.label) end
end)
```

## Transcript grouping and renderers

Transcript history stays flat, but the display can group adjacent matching
blocks. Built-ins group adjacent `read_file`, `grep`, and `glob` calls as soon
as parallel calls appear, plus background-process completion notes. User fold
state is session-local.

Quick display preferences live in `smelt.settings.transcript`:

```lua
smelt.settings.transcript = {
  view = {
    blocks = { thinking = "peek" },
    tools = { read_file = "collapsed", grep = "collapsed", glob = "collapsed" },
    groups = { read_file_batch = "collapsed", grep_batch = "collapsed" },
  },
  limits = { tool_rows = 20, thinking_peek_rows = 4 },
}
```

Register custom display-only groups with `smelt.transcript.groups.register`.
Selectors are declarative so Rust can plan adjacent runs without calling Lua for
every block; the renderer is ordinary Lua layout code.

```lua
local layout = smelt.layout
local defaults = require("smelt.transcript.defaults")

smelt.transcript.groups.register({
  name = "cargo-test-batch",
  cache_key = "my.cargo-test-batch:v1",
  min = 2,
  default_view = "collapsed",
  selector = {
    kind = "tool",
    name = "bash",
    terminal = true,
    fields = { ["args.description"] = "Run cargo tests" },
  },
  render = function(group, ctx)
    local summary = layout.text("ran " .. tostring(group.child_count) .. " test commands")
    if group.view_state ~= "expanded" then return summary end
    return layout.vbox({ summary, defaults.render_group_children(group, ctx) })
  end,
})
```

Use `bucket = "args.package"` or `bucket = { "name", "args.package" }` when one
rule matches several categories but should split adjacent runs by field value.
Omit `cache_key` only for intentionally uncached renderers; otherwise bump it
whenever output can change across restarts.

## Custom tools

`smelt.tools.register({ name, execute, ... })` exposes a tool to the model. Only
`name` and `execute` are required; the rest of `smelt.tools.ToolDef` is optional
metadata that controls summaries, approvals, and per-mode behaviour. Tool
transcript rendering is handled by the root transcript renderer; customize it
with `smelt.transcript.extend_renderer` when a plugin needs custom display.

```lua
smelt.tools.register({
  name        = "present_plan",
  description = "Present a written plan for the user to save as a draft, approve, or approve in apply mode.",
  modes       = { "plan" },           -- only registered in plan mode
  parameters  = {
    type = "object",
    properties = {
      title     = { type = "string", description = "..." },
      slug      = { type = "string", description = "..." },
      plan      = { type = "string", description = "..." },
      plan_path = { type = "string", description = "..." },
    },
  },
  permission_defaults = { plan = "allow" },
  summary  = function(args) return args.title or args.plan_path or "plan" end,
  execute  = function(args)
    local action = smelt.dialog.open({ ... }) -- yields, allowed inside execute
    if action == "approve" then smelt.mode("normal") end
    if action == "apply" then smelt.mode("apply") end
    return "ok"
  end,
})
```

Return either a plain string (success) or `{ content, is_error }`. From inside
`execute` you can also:

- **Chain another tool** with `smelt.tools.call("name", args, parent_call_id)`;
  it yields until the child resolves and returns its `{ content, is_error }`.
- **Park on user input** with `smelt.task.wait(id)`, resumed later by
  `smelt.task.resume(id, value)` from a key handler, event subscriber, etc.

Set `override = true` to replace a built-in tool of the same name. Use
`smelt.tools.unregister(name)` to take it back out.

To customize how a tool appears in the transcript, extend the transcript
renderer and dispatch on `block.kind == "tool"` / `block.name`; return any
`smelt.layout` tree.

For tool-authoring conventions (parameter shape, `summary`, `approval_patterns`,
`preflight`, `paths_for_workspace`), the implementations under
[`runtime/lua/smelt/tools/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/tools)
are the source of truth.

## Statusline sources

Plugins can append segments to the statusline. The handler is called once per
refresh with the current snapshot and returns one segment or a list of segments:

```lua
local statusline = require("smelt.statusline")

statusline.add("clock", function()
  return { {
    text = os.date(" %H:%M "),
    style = { fg = { ansi = 245 } },
    priority = 2,
    align_right = true,
  } }
end)
```

`statusline.remove(name)` removes it. Built-in segments (slug, vim mode, agent
mode, running processes, permission state, and cursor position) keep rendering
alongside whatever you add.

## String-literal aliases

String parameters typed as `smelt.<namespace>.<Name>` accept a closed set of
labels. The IDE shows them in autocomplete and rejects typos. Closed aliases
require canonical names only: `smelt.vim.set_mode("normal")` works,
`smelt.vim.set_mode("n")` does not (short forms `"n"`, `"i"`, `"v"`, `"V"`, and
PascalCase variants like `"Insert"` are not accepted). Open aliases (e.g.
[`smelt.signal.Name`](../reference/api/types.md#smeltsignalname)) keep accepting
any string and just expose well-known names as completion hints.
