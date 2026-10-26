# Tools and transcript

## Custom tools

`smelt.tools.register({ name, execute, ... })` gives the agent a new tool. Only
`name` and `execute` are required; the other fields of `smelt.tools.ToolDef`
control summaries, approvals and per-mode behavior.

```lua
smelt.tools.register({
  name        = "present_plan",
  description = "Present a written plan for the user to save as a draft, approve, or approve in apply mode.",
  modes       = { "plan" },           -- only registered in plan mode
  parameters  = {
    type = "object",
    properties = {
      title     = { type = "string", description = "..." },
      plan      = { type = "string", description = "..." },
    },
  },
  permission_defaults = { plan = "allow" },
  summary  = function(args) return args.title or "plan" end,
  execute  = function(args)
    local action = smelt.dialog.open({ ... }) -- may yield inside execute
    if action == "approve" then smelt.mode.set("normal") end
    if action == "apply" then smelt.mode.set("apply") end
    return "ok"
  end,
})
```

`execute` returns a string (success) or `{ content, is_error }`. Inside it you can
also:

- chain another tool with `smelt.tools.call("name", args, parent_call_id)`, which
  yields until the child finishes;
- wait for user input with `smelt.task.wait(id)`, resumed later by
  `smelt.task.resume(id, value)`.

Set `override = true` to replace a built-in tool with the same name, and use
`smelt.tools.unregister(name)` to remove yours. The tools under
[`runtime/lua/smelt/tools/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/tools)
show the conventions for parameters, `summary`, `approval_patterns`, `preflight`
and `paths_for_workspace`.

## Tool display

Presentation is registered separately from execution. Provide focused callbacks
and the default renderer keeps its status marker, duration and body layout:

```lua
smelt.transcript.register_tool("present_plan", {
  cache_key = "my.present-plan-presentation:v1",
  title = function(tool, ctx)
    return tool.args.title or "plan"
  end,
  body = function(tool, ctx)
    return smelt.layout.markdown(tool.args.plan or tool.output.content or "")
  end,
  compact = function(tool, ctx)
    return tool.args.plan_path
  end,
})
```

Use `render` instead when you want to draw everything yourself, including the
status marker and timings:

```lua
smelt.transcript.register_tool("present_plan", {
  cache_key = "my.present-plan-complete:v1",
  render = function(tool, ctx)
    return smelt.layout.vbox({
      smelt.layout.text("plan: " .. (tool.args.title or "untitled")),
      smelt.layout.markdown(tool.args.plan or ""),
    })
  end,
})
```

`cache_key` lets smelt cache your output across restarts. Bump it whenever the
output could change, or omit it to opt out of caching.

For middleware that wraps several tools, groups or ordinary blocks, use
`smelt.transcript.extend_renderer` (see below).

## Transcript groups

The transcript stays flat in history, but adjacent related blocks can display as
one group. Built in:

- `explore`: `read_file`, `grep`, `glob`, `outline`, `find_symbol`
- `lsp`: `inspect_symbol`, `inspect_symbol_at`, `find_definition`,
  `find_references`, `diagnostics`
- `web`: adjacent `web_search` and `web_fetch`

Set their default fold state with
[`smelt.settings.transcript`](../customize/settings.md#transcript). Register your
own groups with `smelt.transcript.groups.register` and render them with
`smelt.transcript.extend_renderer`:

```lua
local layout = smelt.layout

smelt.transcript.groups.register({
  name = "cargo-test-batch",
  cache_key = "my.cargo-test-batch-plan:v1",
  min = 2,
  default_view = "collapsed",
  selector = {
    kind = "tool",
    name = "bash",
    terminal = true,
    fields = { ["args.description"] = "Run cargo tests" },
  },
})

smelt.transcript.extend_renderer("my.cargo-test-batch", function(next, node, ctx)
  if node.kind ~= "group" or node.name ~= "cargo-test-batch" then
    return next(node, ctx)
  end

  local summary = layout.text("ran " .. tostring(node.child_count) .. " test commands")
  if ctx.view_state ~= "expanded" then return summary end
  return layout.vbox({ summary, layout.group_children() })
end, { cache_key = "my.cargo-test-batch-renderer:v1" })
```

Selectors are declarative so smelt can plan groups without calling Lua for every
block. Add `bucket = "args.package"` (or a list of fields) to split adjacent runs
by field value.

## Language-server tools

The opt-in `smelt.plugins.lsp` plugin gives the agent navigation, diagnostics and
rename tools backed by standard stdio language servers. The server executables
must already be on `PATH`.

```lua
local lsp = require("smelt.plugins.lsp")

lsp.setup({
  servers = {
    rust = {
      cmd = { "rust-analyzer" },
      extensions = { "rs" },
      language_id = "rust",
      root_markers = { "Cargo.toml", ".git" },
    },
  },
})
```

Each key under `servers` is a label of your choosing.

| Field | Description |
| ----- | ----------- |
| `cmd` | Executable and arguments (required) |
| `extensions` | File extensions handled, without dots |
| `language_id` | LSP language id sent for opened documents |
| `root_markers` | Files or directories searched upward to find the project root |
| `init_timeout_ms` | Initialize timeout (default `120000`) |
| `request_timeout_ms` | Per-request timeout (default `30000`) |
| `startup_wait_ms` | Wait for a starting server or fresh diagnostics (default `5000`) |
| `initialization_options` | JSON value sent in the initialize request |
| `settings` | JSON workspace configuration sent after initialization |

The plugin adds `language_server_status`, `outline`, `find_symbol`,
`inspect_symbol`, `inspect_symbol_at`, `find_definition`, `find_references`,
`diagnostics`, `preview_rename` and `rename_symbol`. Servers start lazily per
file and project root. Read-only tools are allowed by default. `rename_symbol` is
allowed in Normal, denied in Plan, asks in Apply and is allowed in Yolo.

## Provider middleware

Observe or rewrite assembled assistant responses:

```lua
smelt.provider.middleware({
  on_response = function(message)
    -- inspect, or return a replacement assistant message
  end,
})
```

Hooks run in registration order and each sees the previous hook's replacement.
To watch streaming tokens without changing anything, subscribe to the
`stream_delta` event. See [`smelt.provider`](../reference/api/provider.md).
