---
title: "Plugin UI: overlays, dialogs and bars"
description: "Build overlays, dialogs, pickers, resizable splits, statusline segments and prompt bar items for the terminal UI of a smelt Lua plugin."
---

# UI

All of these are UiHost APIs: they need the terminal UI and raise in headless
mode.

## Overlays

An overlay is a floating or docked window. Create a buffer, attach it to a
window, and mount the window in an overlay:

```lua
local buf = smelt.buf.new()
local win = smelt.win.new(buf, { surface = "inert" })

smelt.overlay.new({
  title     = { { text = " perf ", bold = true } },
  anchor    = "screen_at",
  corner    = "ne",
  width     = 44,
  height    = 14,
  modal     = false,
  draggable = true,
  layout    = smelt.ui.layout.leaf(win),
})
```

**Anchor** decides where the overlay lives:

| Anchor | Placement |
| ------ | --------- |
| `dock_bottom` (default) | Docked above the statusline |
| `dock_top`, `dock_left`, `dock_right` | Docked to that edge (statusline row stays reserved) |
| `center` | Centered on screen |
| `screen_at` | Absolute; pair with `corner`, `row`, `col` |
| `win` | Attached to another window; pair with `target`, `attach`, `row_offset`, `col_offset` |

**Size**: `width` and `height` are fixed; `max_width` and `max_height` shrink to
fit up to a cap. Don't set both fixed and max on one axis. Values are a number of
cells, a percentage string like `"50%"`, or `"fill"`.

## Dialogs

For a modal with a markdown panel, an option list and a text input,
`smelt.dialog.open` is the higher-level API and returns the user's choice. The
bundled dialogs in
[`runtime/lua/smelt/dialogs/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/dialogs)
(`confirm`, `permissions`, `resume`, `rewind`) are the reference
implementations.

Dialog height is either `height = "N%"` (fixed) or `max_height = "N%"` (shrink
to fit, capped). With neither, the dialog fits its content. Setting both is an
error.

## Layouts and resizable splits

Compose windows with `hbox`, `vbox` and `leaf` nodes. `hsplit(first, second,
opts)` puts two subtrees side by side and `vsplit` stacks them, each with a
draggable divider.

```lua
local layout = smelt.ui.layout
local files = smelt.win.new(smelt.buf.new(), { surface = "list_inert" })
local changes = smelt.win.new(smelt.buf.new(), { surface = "readonly_text" })
local body = layout.hsplit(layout.leaf(files), layout.leaf(changes), {
  size = 34, min_first = 26, min_second = 32,
})
smelt.dialog.new({ layout = body, height = 20, focus = files })
```

The same `body` works in `smelt.overlay.new({ layout = body })`. `size` is the
first pane's initial extent in cells, a percentage, or `ratio:N/M` (default
`"50%"`). By default a user-resized split keeps its ratio when the terminal
changes; pass `resize = "cells"` to keep a fixed width instead.

If you rebuild the layout from a composer function, create the split once with
`layout.split("horizontal", opts)` and call `split:layout(first, second)` inside
the composer, so the user's chosen size is kept. `split:size()` and
`split:set_size()` save and restore it.

`layout.frame(body, { border = "single", padding = 1, title = "details" })` adds
chrome around any subtree.

`win:resize("width", 4)` and `win:resize("height", -1)` resize the nearest split
(or the containing overlay), and `win:equalize()` balances splits. The built-in
keys are `Ctrl-W >`, `<`, `+`, `-` and `=`.

`smelt.dialog.new` and `smelt.dialog.open` accept either a composed `layout` or
the simpler `panels` / `bottom_panels` recipe, not both.

### The main screen

`smelt.ui.layout.set(fn)` replaces the composer for the whole screen. The
default one is `require("smelt.layout").compose`, so a plugin can wrap it
rather than rebuild it, for example to put a sidebar beside everything:

```lua
local layout = smelt.ui.layout
local default = require("smelt.layout").compose
local side = smelt.win.new(smelt.buf.new(), { surface = "list_inert" })
layout.set(function(state)
  return layout.hbox({
    { default(state), width = "fill" },
    { layout.leaf(side, { border = "rounded", title = " notes " }), width = 30 },
  })
end)
```

The [changes sidebar](gallery.md#changes-sidebar) in the gallery is a complete
example.

## Large read-only views

For big read-only content, give a window a document instead of filling a buffer
with every line. Only the visible rows are materialized, so a million-line diff
stays responsive.

```lua
smelt.spawn(function()
  local snapshot, err = smelt.git.diff()
  if not snapshot then smelt.notify.error(err); return end
  local buf = smelt.buf.new()
  local win = smelt.win.new(buf, { surface = "readonly_text", vim_enabled = true })
  win:document(snapshot.diff:document())
  smelt.overlay.new({
    anchor = "center", width = "90%", height = "80%", modal = true,
    layout = smelt.ui.layout.leaf(win),
  })
  win:focus()
end)
```

- `smelt.document.text(text)` builds a document from plain text;
  `smelt.diff.parse(patch)` from a unified patch. `smelt.git.diff()` loads and
  indexes off the UI thread.
- A document can back several windows, each with its own scroll and cursor.
  `win:document(nil)` restores the original buffer.
- Syntax highlighting arrives in the background; text and navigation work
  immediately.
- `smelt.git.index(diff, index, action)` stages or unstages one file
  (`"stage"`, `"unstage"` or `"toggle"`) without touching the worktree, and
  returns a fresh snapshot.

The bundled
[`smelt.plugins.diff`](https://github.com/leonardcser/smelt/blob/main/runtime/lua/smelt/plugins/diff.lua)
plugin shows the full pattern with a file tree, folding and refresh.

## Pickers

`smelt.picker.open(opts)` asks the user to choose one item. Items are strings or
`{ label, description, ansi_color, search_terms }` records. With `placement =
"prompt_docked"` the prompt filters them using `smelt.fuzzy.rank`. It returns
`{ index, item, action }`, or `nil` when dismissed.

```lua
smelt.spawn(function()
  local choice = smelt.picker.open({
    items = { "first", "second", "third" },
    placement = "prompt_docked",
  })
  if choice then smelt.log.info("picked " .. choice.item) end
end)
```

## Window events and marks

To decorate an existing buffer, subscribe to window events and draw marks in a
namespace you own:

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

Event names and `mark` options are type-checked in your editor.

## Paint leaves and pointer events

For canvas-like overlays that don't need a buffer, register a paint function and
handle pointer events directly. The leaf that received `press` keeps receiving
`drag` and `release` even if the pointer leaves its rectangle.

```lua
local paint = smelt.paint.register(draw_fn, { name = "myplugin.paint" })
paint:on("press",   function(ev) smelt.notify.info("down @ "..ev.row..","..ev.col) end)
paint:on("drag",    function(ev) ... end)
paint:on("release", function(ev) ... end)
```

## Statusline and prompt bars

The statusline and the two rules around the prompt are bars. A bar holds named
sources: functions that run on every repaint and return a list of items. smelt
lays the items out by priority and drops the least important ones when the line
is narrow.

| Bar | Where | Built-in sources |
| --- | ----- | ---------------- |
| `smelt.statusline` | The bottom row | `core` |
| `smelt.prompt_bar.top` | The rule above the prompt | `pause`, `indicator`, `elapsed`, `retry` on the left; `model`, `tokens`, `cost` on the right |
| `smelt.prompt_bar.bottom` | The rule below the prompt | None |

```lua
smelt.statusline.add("clock", function()
  return { text = os.date("%H:%M"), align_right = true, priority = 2 }
end)

smelt.prompt_bar.bottom.add("hint", function()
  return { text = "ctrl+s stash", style = { fg = "Comment" }, align_right = true }
end)
```

A source returns one item or a list of them. Each item is a table:

| Field | Meaning |
| ----- | ------- |
| `text` | What to show. Items with empty text are skipped |
| `runs` | A list of `{ text, style }` pieces instead of `text`, for coloring each character; the pieces drop together |
| `style` | `fg`, `bg`, `hl_group`, `bold`, `dim`, `italic`; colors are theme group names or `{ r, g, b }` |
| `priority` | Higher drops first when the line is narrow; `0` (the default) never drops |
| `align_right` | Pack against the right edge |
| `separated` | Put a `·` between this item and the previous separated one |
| `truncatable`, `truncate` | Shrink the text with `…` before dropping it, from the end or the `"middle"` |
| `selectable` | Include the text in drag-copy (default `true`) |

Neighbouring items get one space between them. The prompt bars draw the rule
and its edges themselves, so a source only returns what sits on it.

Adding a name that already exists replaces that source where it stood, so
`add("model", fn)` swaps the model display and keeps its place. `remove(name)`
drops a source, and `add` returns a `Reg` that removes the one it added.
`builtin(name)` returns a built-in source, so a replacement can fall back to it
for the cases it does not draw:

```lua
local fallback = smelt.prompt_bar.top.builtin("indicator")

smelt.prompt_bar.top.add("indicator", function()
  if smelt.signal.get("work_state") ~= "working" then return fallback() end
  return { text = "thinking", style = { fg = "SmeltAccent" }, priority = 1 }
end)

smelt.prompt_bar.top.remove("cost")
```

A bar repaints only when something it shows changes. A source that animates
draws from `smelt.time.now_ms()` and asks for its next frame on every paint
with `invalidate_after(ms)`, the way the built-in `indicator` breathes while the agent
works and cools its dot after. A source that depends on state of its own calls
`invalidate()` on its bar when that state changes. A failing source is reported
on stderr and skipped.
