---
title: "Transcript rendering"
description: "Customize tool display, compose semantic transcript content and group related blocks with Lua."
---

# Transcript rendering

Transcript presentation is separate from [tool execution](tools-and-agent.md).
Use the smallest change that fits:

| Goal | API |
| ---- | --- |
| Change one tool's title or body | `smelt.transcript.register_tool` |
| Replace a block or semantic part | `smelt.transcript.set_renderer` |
| Wrap a renderer | `smelt.transcript.extend_renderer` |
| Render a part's current composition | `smelt.transcript.render_part` |
| Group adjacent related blocks | `smelt.transcript.groups.register` |

These renderers return `smelt.layout` nodes, not the `smelt.ui.layout` window
trees used by [dialogs and overlays](ui.md). See the
[transcript reference](../reference/api/transcript.md) for signatures.

## Tool display

Register focused callbacks to keep the default marker, timings and body layout:

```lua
smelt.transcript.register_tool("present_plan", {
  cache_key = "my.present-plan:v1",
  title = function(tool, ctx)
    return tool.args.title or "plan"
  end,
  body = function(tool, ctx)
    return smelt.layout.markdown(tool.args.plan or "")
  end,
  compact = function(tool, ctx)
    return tool.args.plan_path
  end,
})
```

Use `render` instead of these callbacks only when replacing the complete tool
view, including status and timings:

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

## Renderer parts

`set_renderer`, `get_renderer` and `extend_renderer` target the root block
renderer by default. Pass `part` to change a stage such as `"user"`, `"thinking"`
or `"tool_header"`. Tool header changes also apply to grouped tool headers.
See [`RendererOptions`](../reference/api/types.md#smelttranscriptrendereroptions)
for supported part names.

`render_part(name, block, ctx)` uses the current composition, including later
middleware. `get_renderer(name)` captures the composition at the time of the call.

Semantic content is separate from chrome. `tool_header_content` keeps the tool
name, provider namespace, summary, duration, timestamps and row limits. Add a
custom marker without rebuilding that content:

```lua
local transcript, layout = smelt.transcript, smelt.layout

local chrome = transcript.extend_renderer("my.tool-marker", function(_, tool, ctx)
  local content = transcript.render_part("tool_header_content", tool, ctx)
  return layout.row_prefix(content, {
    first = { { text = "◆ ", hl = "SmeltAccent", selectable = false } },
    rest = { { text = "  ", selectable = false } },
  })
end, { part = "tool_header", cache_key = "my.tool-marker:v1" })

-- chrome:remove() removes this override.
```

To omit the marker, set the header renderer to its content renderer:

```lua
local transcript = smelt.transcript
transcript.set_renderer(function(block, ctx)
  return transcript.render_part("tool_header_content", block, ctx)
end, { part = "tool_header", cache_key = "my.unprefixed-header:v1" })
```

Likewise, `user_content` provides styled user text without the default panel or
timestamp. Reuse it in a bubble view:

```lua
local transcript, layout = smelt.transcript, smelt.layout
transcript.set_renderer(function(block, ctx)
  local content = transcript.render_part("user_content", block, ctx)
  return layout.panel(content, { hl = "SmeltUserBg", padding = 1 })
end, { part = "user", cache_key = "my.bubble:v1" })
```

Middleware delegates with `next(block, ctx)`; later registrations run first.
Names are scoped to the part. Replacing a base keeps its middleware. A `Reg`
removes only its own registration, not a newer replacement with the same name.
To wrap a base with `set_renderer`, capture `get_renderer` first so the new
renderer does not call itself.

### Caching and invalidation

For both tool presentations and renderer parts, `cache_key` permits persisted
layouts. Bump it when output changes across restarts. Omit it for dynamic plugin
state and call `smelt.transcript.invalidate_renderer()` when that state changes.
Part changes invalidate standalone and grouped layouts.

## Transcript groups

History stays flat, but adjacent related blocks can display as one group.
Built-in groups are:

- `explore`: `read_file`, `grep`, `glob`, `outline`, `find_symbol`
- `lsp`: `inspect_symbol`, `inspect_symbol_at`, `find_definition`, `find_references`, `diagnostics`
- `web`: `web_search`, `web_fetch`

Use [transcript settings](../customize/settings.md#transcript) to set default
fold states or disable a group. For a custom group, register a declarative
selector and a renderer for the `group` part:

```lua
local transcript, layout = smelt.transcript, smelt.layout
transcript.groups.register({
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

transcript.extend_renderer("my.cargo-test-batch", function(next, node, ctx)
  if node.name ~= "cargo-test-batch" then return next(node, ctx) end
  local summary = layout.text("ran " .. tostring(node.child_count) .. " test commands")
  if ctx.view_state ~= "expanded" then return summary end
  return layout.vbox({ summary, layout.group_children() })
end, { part = "group", cache_key = "my.cargo-test-batch-renderer:v1" })
```

Selectors let smelt plan groups without calling Lua for every block. Add
`bucket = "args.package"` or a list of fields to split adjacent runs by value.
See the [group reference](../reference/api/transcript_groups.md).
