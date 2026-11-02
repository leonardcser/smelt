---
title: "Bars and prompt layout"
description: "Customize statuslines, prompt bars and transcript jump pills with reusable sources or layout views."
---

# Bars and prompt layout

Bars display named sources as rows of items. The statusline, headerline and
prompt bars all use the same `smelt.bar.Bar` API, available in the terminal UI.

| Bar | Position | Built-in sources |
| --- | -------- | ---------------- |
| `smelt.statusline` | Bottom of the screen | `slug`, `vim_mode`, `agent_mode`, `tps`, `permission`, `keymap`, `procs`, `worktree`, `vim_pending`, `cursor` |
| `smelt.headerline` | Above the transcript | `goal` |
| `smelt.prompt.aux` | Between transcript and prompt; notifications float over it | `tip` |
| `smelt.prompt.queue` | Above the prompt, one row per queued message | `stash`, `queued` |
| `smelt.prompt.top` | Rule above the input | `pause`, `indicator`, `elapsed`, `retry`, `model`, `tokens`, `cost` |
| `smelt.prompt.bottom` | Rule below the input | None |

## Add, wrap or move a source

A source returns an item or a list of items. smelt places them by priority and
drops less important items when space is limited. Add a second statusline row
by setting `row`:

```lua
smelt.statusline:add("cwd", function()
  return { text = smelt.session.cwd(), row = 2, style = "Comment" }
end)
```

Adding an existing name replaces that source in place. `remove(name)` removes
one; `clear()` removes all. The `Reg` returned by `add` removes only that
registration. Use `builtin(name)` to reuse a built-in source:

```lua
local top = smelt.prompt.top
smelt.statusline:add("model", top:builtin("model"))
top:remove("model")
```

To wrap a source, keep its signal dependencies with its renderer:

```lua
local fallback = smelt.prompt.top:builtin("indicator")
smelt.prompt.top:add("indicator", {
  signals = fallback.signals,
  render = function(ctx)
    if smelt.signal.get("work_state") ~= "working" then
      return fallback.render(ctx)
    end
    return { text = "thinking", style = "SmeltAccent", priority = 1 }
  end,
})
```

Sources may declare `signals = { ... }` to repaint when values change. An entry
`{ name, project }` compares `project(value)` instead of the whole value. These
subscriptions follow the registration when it moves or is replaced.

For plugin-owned state, call `bar:invalidate()` after a change. Animated sources
request their next frame with `ctx.invalidate_after(ms)` and usually draw from
`smelt.time.monotonic_ms()`. The built-in indicator uses wall time deliberately
to synchronize across smelt windows.

The render context supplies `width`. If a strip has too few rows, `ctx.rows`
is the remaining budget after earlier sources; otherwise it is nil. This lets
the queue fold old messages while preserving the stash row. A failing source
is skipped and produces a notification.

See [`Item`](../reference/api/types.md#smeltbaritem) for item fields and
[`Bar`](../reference/api/types.md#smeltbarbar) for methods. Use `runs` for styled
spans, `row` for multiple rows, and `align_right` for right-aligned content.
`add(name, source, opts)` can override placement without changing the source.

## Change bar chrome

`configure(opts)` patches the current look. `reset(key)` restores one default;
`reset()` restores all look defaults and source rendering without removing
sources. `false` is a value, not a reset sentinel. `config()` returns the current
look. Invalid options fail before any change.

```lua
smelt.statusline:configure({ separator = " | " })
smelt.prompt.bottom:configure({ rule = "╌", rule_style = "Comment" })
```

`layout = "strip"` fills rows; `"rule"` draws lines around the items. Set
`visible = false` to hide a bar and free its rows without removing its sources.
See [`Config`](../reference/api/types.md#smeltbarconfig) for all look options.

## Reshape the prompt

Source-rendered bars expose `bar.win`. Hosted bars have no window while hidden
or using a custom view. Window styles fill content and padding; bar options
control rules and separators. Together they can form a filled prompt block:

```lua
local prompt = smelt.prompt
prompt.win:configure({ style = "SmeltUserBg" })
for _, bar in ipairs({ prompt.top, prompt.bottom }) do
  bar:configure({ layout = "strip", bg = "SmeltUserBg" })
  bar.win:configure({ pad_left = 1, pad_right = 1 })
end
```

For a shell-style input marker, reserve padding for its width:

```lua
smelt.prompt.win:configure({
  pad_left = 2,
  prefix = { first = "> ", style = "SmeltAccent" },
})
```

`prefix.rest` applies to later input rows. Prefixes wider than the left padding
are clipped.

For a one-row prompt, hide the top and bottom bars and move useful sources to
the statusline. Queued messages still appear in `smelt.prompt.queue`. Hiding
the top bar also removes the prompt's mouse-resize handle; assign it to another
window with `smelt.prompt.set_resize_handle(win, enabled)`.

## Replace a bar with a layout

Use `set_view(factory)` when items are not enough. The factory returns a measured
`smelt.ui.layout` tree. Create fresh leaves inside it; the bar owns their
resources, timers and subscriptions, and releases them on replacement or
teardown. Names are local to the view. Keep shared buffers outside the factory.

```lua
smelt.statusline:set_view(function(ctx)
  local paint = smelt.paint.register(function(slice, frame)
    for x = 0, slice:width() - 1 do
      local hue = (x * 4 + frame.now_ms // 20) % 256
      slice:set(x, 0, " ", { bg = { hue, 80, 255 - hue } })
    end
    ctx.invalidate_after(33)
  end)
  return smelt.ui.layout.leaf(paint, { measure = { 0, 1 } })
end)
```

Failed construction keeps the current view. Open dialogs and overlays from
event handlers, not during construction. `ctx.width` is the construction
budget; use the paint slice's width when drawing. Paint leaves need an explicit
`measure`; windows measure their content. Natural size includes borders and
padding. `smelt.ui.layout.size(node, width, height)` measures without drawing.
Pass `nil` to `set_view` to restore sources. See [Paint](paint.md) for drawing
and animation.

## Create a bar

Create a named bar and place `bar:node()` in the main layout:

```lua
local tabs = smelt.bar.new({ name = "tabs", bg = "SmeltStatusBg", min_rows = 0 })
tabs:add("sessions", function() return { text = " main " } end)

local default = require("smelt.layout").compose
smelt.ui.layout.set(function(state)
  local node = tabs:node()
  return smelt.ui.layout.vbox({
    { height = "fit", node },
    { height = "fill", default(state) },
  })
end)
```

`bar:node()` also returns the requested row count. Constructor `sources` become
available through `builtin(name)`; `add_builtin(name, source)` supplies a
reusable source to an existing bar. See [`Opts`](../reference/api/types.md#smeltbaropts).

For custom screen layouts, `smelt.prompt.node({ input_rows, max_rows })` returns
the whole prompt block and its row count. See [Layouts](ui.md#layouts-and-resizable-splits).

## Jump pills

`smelt.transcript.pills.top` and `.bottom` are bars over the transcript. Both
have a `label` source; navigation and cursor avoidance stay built in.

```lua
local bottom = smelt.transcript.pills.bottom
bottom:add("label", function() return { text = " ↓ latest " } end)
bottom:configure({ style = {
  fg = "SmeltAccent", bg = "SmeltScrollPillBg", bold = true,
} })
```

Their custom-view context stays live and exposes `{ label, target, view }` in
`ctx.state`. Every leaf in the view inherits the bar's press action:

```lua
smelt.transcript.pills.top:set_view(function(ctx)
  local paint = smelt.paint.register(function(slice)
    slice:put_str(0, 0, " ↑ " .. ctx.state.label, "SmeltAccent")
  end)
  return smelt.ui.layout.leaf(paint, { measure = { 40, 1 } })
end)
```

Restore the label with `bar:add("label", bar:builtin("label"))`. Disabling the
bundled `scroll_pills` plugin disables both controls. Changing labels or styles
does not enable paint, pointer tracking or screen history.

Top-anchored overlays can set `reserve = true` to keep navigation targets below
them. Reservations use current geometry without adding overlapping heights or
moving overlays. `smelt.transcript.top_inset(rows)` sets an independent minimum.
