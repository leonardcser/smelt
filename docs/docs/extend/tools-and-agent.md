---
title: "Tools and provider hooks"
description: "Register agent tools, configure language servers and rewrite provider responses from Lua."
---

# Tools and provider hooks

## Custom tools

`smelt.tools.register({ name, execute, ... })` gives the agent a new tool. Only
`name` and `execute` are required; the other fields of `smelt.tools.ToolDef`
control access, summaries and per-mode behavior.

```lua
smelt.tools.register({
  name        = "present_plan",
  description = "Present a plan for review.",
  modes       = { "plan" },
  parameters  = {
    type = "object",
    properties = {
      title = { type = "string", description = "Short plan title" },
      plan = { type = "string", description = "Plan in Markdown" },
    },
    required = { "title", "plan" },
  },
  access = "read",
  summary = function(args) return args.title end,
  execute = function(args) return args.plan end,
})
```

`access` tells the [permission policy](../reference/permissions.md) what a call
may change: `"read"`, `"write"` (the default), `"full"`, or a function
`access(args, mode)`. A tool should also declare `paths(args)`, the paths a call
reads (for a `"read"` tool) or changes, so smelt can check them against the
[path rules](../reference/permissions.md#path-rules). A `"write"` call without
`paths`, or with a path outside the workspace and `"allow"` paths, counts as
`"full"`:

```lua
  access = "write",
  paths = function(args) return { args.path } end,
```

`execute` returns a string (success) or `{ content, is_error }`. It runs in a
coroutine, so it can open a [dialog](ui.md#dialogs), call another tool with
`smelt.tools.call("name", args, parent_call_id)`, or wait for another task. See
[Tasks and I/O](async.md) for waiting and cancellation.

The commands a call starts with `smelt.process` or `smelt.grep` run in the
call's [sandbox](../reference/permissions.md#the-sandbox), and
`smelt.fs.glob_async` leaves out the files the call may not read. Pass
`sandbox = false` to `smelt.process.run` for a trusted helper that must run
unconfined.

Set `override = true` to replace a built-in tool with the same name, and use
`smelt.tools.unregister(name)` to remove yours. The tools under
[`runtime/lua/smelt/tools/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/tools)
show the conventions for parameters, `summary`, `access`, `paths` and
`preflight`.

Tool execution and display are independent. See [Transcript rendering](transcript.md)
to change a tool's title or body, compose block renderers, or group related calls.

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
allowed in Read, denied in Plan, asks in Write and is allowed in Yolo.

## Provider middleware

Observe or rewrite assembled assistant responses:

```lua
smelt.provider.middleware({
  on_response = function(message)
    -- inspect, or return a replacement assistant message
  end,
})
```

Hooks run in registration order; each sees the previous replacement. The final
message determines visible output, history and tool execution. Removing all
tool calls makes it a terminal response. If it contains neither text nor calls,
smelt retries up to the empty-response limit, then reports an error.

To observe tokens without rewriting, subscribe to `stream_delta`. See the
[provider reference](../reference/api/provider.md) for hook options.
