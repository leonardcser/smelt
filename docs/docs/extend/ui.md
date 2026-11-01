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
`{ label, description, prefix_color, search_terms }` records. With `placement =
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

## Paint

A paint is a Lua function that draws cells directly. There are two kinds:

- `smelt.paint.register(fn, opts)` returns a handle you place in a layout or
  overlay like a window. `fn` draws that leaf's rectangle.
- `smelt.paint.frame(fn, opts)` draws over the whole finished screen, just
  before smelt sends the frame. Use it for screen-wide effects: fades, color
  filters, melting text, a mouse trail.

Both call `fn(slice, ctx)`. Coordinates are `(x, y)` cells from the top-left.
They may be negative or past the edge: writes outside the slice are dropped and
reads return `nil`, so a moving effect never clamps by hand. The slice already
holds what was painted below it, so a paint can read cells and draw over them.
A write whose style leaves `bg` unset keeps the background underneath; pass
`bg = false` for the terminal's default background instead. Reads give
`bg = false` for that default, so a cell read with `get` and written elsewhere
with `set` looks the same there.

| Method | Does |
| ------ | ---- |
| `get(x, y)` | Read a cell as `{ ch, width, style, image? }` |
| `set(x, y, ch, style)` | Write one grapheme |
| `put_str(x, y, text, style)` | Write a string, clipped at the edges |
| `fill(x, y, w, h, ch, style)` | Fill a rectangle |
| `set_style(x, y, w, h, patch)` | Change only the style of a rectangle, keeping the glyphs |
| `map(fn(cell, x, y))` | Visit every cell; return `{ ch?, style? }` to replace it |
| `recolor(fn(r, g, b, layer), opts)` | Pass every color through `fn`, once per distinct color |
| `snapshot()` | Copy the slice into a `Snapshot` you can keep |
| `blit(snapshot, x, y)` | Draw a snapshot back, shifted by `(x, y)` |
| `set_cursor(x, y)` | Move the hidden terminal cursor, where input-method text appears |

`ctx` holds `x`, `y` (the slice's screen position), `now_ms` and `frame` for
animation, `focused`, and the terminal size. `ctx.now_ms` is the same monotonic
clock as `smelt.time.monotonic_ms()`, so a timer and a paint agree on time;
`smelt.time.now_ms()` is wall time, for timestamps only. `ctx.focused` tells
whether keystrokes reach the paint: for a frame paint, whether the terminal has
focus, so an effect can dim the screen while you are away. A frame paint also
gets `ctx.cursor = { x, y }` while a cursor is visible, so an effect can follow
it. `smelt.paint.Ctx` in your editor lists every field.

### Colors

Every style in smelt takes the same colors:

| Form | Color |
| ---- | ----- |
| `{ r, g, b }` | RGB. Channels may be fractions; they are rounded and kept in 0-255 |
| `"#rrggbb"` | RGB |
| `0` to `255` | An ANSI 256-color slot |
| `"red"`, `"darkblue"`, ... | One of the 16 ANSI colors |
| `"Comment"`, `"SmeltAccent"`, ... | The color of a theme group; names start with a capital |
| `false` | The terminal's default |

Colors read back from smelt, from `slice:get` or `smelt.theme.get`, use the
same forms, so you can pass them straight back in.

`recolor` hands `fn` each color as the terminal shows it, `r, g, b` from 0 to
255, with `layer` set to `"fg"` or `"bg"`. Return new channels, or `nil` to
keep the color. It calls `fn` once per distinct color, not once per cell, so a
screen-wide filter costs almost nothing:

```lua
-- Night shift: warm everything on screen.
local warm = smelt.paint.frame(function(slice)
  slice:recolor(function(r, g, b) return r, g * 0.85, b * 0.6 end)
end, { name = "myplugin.nightshift" })

warm:remove() -- stop the effect
```

Cells that use the terminal's default foreground or background keep it unless
you pass `{ defaults = true }`. An explicit color replaces the default, which
makes the background opaque on a translucent terminal.

At startup smelt asks the terminal for its default colors and its 16 ANSI
colors. `smelt.theme.rgb(color, layer)` uses them to turn any color, including
a theme group or `nil` (the default for `layer`), into `{ r, g, b }`. Terminals
that do not answer get the xterm palette. That is all blending needs:

```lua
local function mix(a, b, k)
  return { a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k, a[3] + (b[3] - a[3]) * k }
end
-- Fade a cell's text halfway into its background.
local cell = slice:get(x, y)
local fg = smelt.theme.rgb(cell.style.fg, "fg")
local bg = smelt.theme.rgb(cell.style.bg, "bg")
slice:set_style(x, y, 1, 1, { fg = mix(fg, bg, 0.5) })
```

### Repainting

A paint repaints only when something under it changes. When a timer, command or
event changes what a paint draws, call `handle:invalidate()`. To animate, call
`handle:invalidate_after(ms)` from the paint itself to ask for the next frame.
An effect that stops asking goes idle on its own, and a leaf that is not on
screen does not paint, so it stops asking too. This is the same model the bars
use. The built-in `/donut` is a full example.

```lua
local fx
fx = smelt.paint.frame(function(slice, ctx)
  -- ... draw ...
  if still_moving then fx:invalidate_after(16) end -- about 60 frames a second
end)
```

Frame paints run from the lowest `order` up (default 0), and in registration
order for equal orders. Give a filter that must see everything, such as a
color-blind simulation, a high order.

### Transitions

Pass `history = true` to `smelt.paint.frame` for effects that need to know what
changed:

- `ctx.prev` is a `Snapshot` of the screen as smelt drew it on the previous
  frame, before any frame paint. It is absent on the first frame.
- `ctx.scrolls` lists every window whose text scrolled since then, such as the
  conversation or a long dialog: `{ win, x, y, width, height, dy }`, the window,
  the screen area of its text and the rows it moved up (negative for down).
  Compare `win` with `==`, for example to `smelt.win.transcript()`. Inside that area,
  row `y` shows what `ctx.prev` had at row `y + dy`, unless a window above
  covers it.
- `ctx.damage` lists the cells that changed, as one-row
  `{ x, y, width, height }` runs in reading order. The first frame and a resize
  damage every row. Scrolls are taken out: a cell that only moved with its text
  is not damage, so new text the model writes shows as damage even while the
  view follows it.

To follow content, read `smelt.transcript.visible_blocks()`. Each block has a
screen `rect` and `top`, the block row at `rect.y`, while it is on screen, and
`streaming` while the model is still writing it. All of them match the frame
being painted. So the text the model just wrote is the damage inside a
streaming block:

```lua
-- Fresh ink: what the model writes flashes bright, then settles.
local fx
fx = smelt.paint.frame(function(slice, ctx)
  for _, b in ipairs(smelt.transcript.visible_blocks()) do
    if b.streaming and b.rect then
      for _, d in ipairs(ctx.damage) do
        if d.y >= b.rect.y and d.y < b.rect.y + b.rect.height then
          slice:set_style(d.x, d.y, d.width, 1, { fg = { 255, 236, 170 }, bold = true })
        end
      end
    end
  end
end, { name = "myplugin.freshink", history = true })
```

```lua
-- Dissolve: what disappeared fades out over a few frames.
local fading, fx = {}, nil
fx = smelt.paint.frame(function(slice, ctx)
  for _, r in ipairs(ctx.damage) do
    for x = r.x, r.x + r.width - 1 do
      local old = ctx.prev and ctx.prev:get(x, r.y)
      if old and old.ch ~= " " then fading[#fading + 1] = { x = x, y = r.y, ch = old.ch, t = 4 } end
    end
  end
  local kept = {}
  for _, f in ipairs(fading) do
    if slice:get(f.x, f.y).ch == " " then slice:set(f.x, f.y, f.ch, { dim = true }) end
    f.t = f.t - 1
    if f.t > 0 then kept[#kept + 1] = f end
  end
  fading = kept
  if #fading > 0 then fx:invalidate_after(33) end -- animate only while something fades
end, { name = "myplugin.dissolve", history = true })
```

### Pointer events

Paint leaves and windows receive pointer events through `handle:on(event, fn)`.
Payloads hold leaf-relative `x` and `y`.

| Event | When |
| ----- | ---- |
| `press`, `release`, `drag` | A button goes down, comes up, or moves while held. The leaf that got `press` keeps `drag` and `release` even if the pointer leaves it. Payload adds `button` |
| `enter`, `leave`, `move` | The pointer hovers onto, off, or within the leaf with no button held |

```lua
local hot = false
local button = smelt.paint.register(function(slice)
  slice:put_str(0, 0, " run ", { reverse = hot })
end, { name = "myplugin.button" })
button:on("enter", function() hot = true end)
button:on("leave", function() hot = false end)
button:on("press", function(ev) smelt.notify.info("clicked at " .. ev.x) end)
```

Pass `pointer = true` to repaint on every pointer move and get
`ctx.pointer = { x, y }` (relative to the slice). A frame paint with
`pointer = true` is all a mouse trail needs.

A frame effect that moves cells must also move the mouse. Pass
`map_pointer(x, y, width, height)`, a function from the screen cell the user
points at to the cell smelt laid out there, and clicks, drags, and wheel scrolls land where they look. With several
effects, the top one maps first. `ctx.pointer` stays in screen cells. The
visible caret is an ordinary cell and moves with the others; call
`slice:set_cursor` so input-method text follows it:

```lua
-- Upside down.
smelt.paint.frame(function(slice, ctx)
  local snap, w, h = slice:snapshot(), slice:width(), slice:height()
  for y = 0, h - 1 do
    for x = 0, w - 1 do
      local cell = snap:get(x, h - 1 - y)
      if cell.width > 0 then slice:set(x, y, cell.ch, cell.style) end
    end
  end
  if ctx.cursor then slice:set_cursor(ctx.cursor.x, h - 1 - ctx.cursor.y) end
end, {
  name = "myplugin.flip",
  order = 100,
  map_pointer = function(x, y, _, height) return x, height - 1 - y end,
})
```

## Statusline and prompt bars

The statusline, the headerline and the two rules around the prompt are bars. A
bar holds named sources: functions that run on every repaint and return a list
of items. smelt lays the items out by priority and drops the least important
ones when the line is narrow.

| Bar | Where | Built-in sources |
| --- | ----- | ---------------- |
| `smelt.statusline` | The bottom rows | `core` |
| `smelt.headerline` | Above the transcript, only while a source shows items | `goal` |
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
| `row` | Row of the statusline or headerline, from `1` at the top. The bar grows to fit |
| `fill` | A theme group that styles the empty space of the item's row |

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

A multi-row statusline only needs items with `row` set; the layout makes room
for it in the same frame:

```lua
smelt.statusline.add("cwd", function()
  return { text = smelt.session.cwd(), row = 2, style = { fg = "Comment" } }
end)
```

To draw the statusline or headerline yourself, swap it for any layout node with
`set_view(node, rows)`, and pass `nil` to restore it. A paint leaf can draw
anything, including animation:

```lua
local paint
paint = smelt.paint.register(function(slice, ctx)
  for x = 0, slice:width() - 1 do
    local hue = (x * 4 + ctx.now_ms // 20) % 256
    slice:set(x, 0, " ", { bg = { hue, 80, 255 - hue } })
  end
  paint:invalidate_after(33)
end)
smelt.statusline.set_view(smelt.ui.layout.leaf(paint), 1)
```

A bar repaints only when something it shows changes. A source that animates
draws from `smelt.time.monotonic_ms()` and asks for its next frame on every
paint with `invalidate_after(ms)`, the way the built-in `indicator` breathes while the agent
works and cools its dot after. The `indicator` uses wall time,
`smelt.time.now_ms()`, on purpose: it keeps the animation in step across
several smelt windows. A source that depends on state of its own calls
`invalidate()` on its bar when that state changes. A failing source is reported
on stderr and skipped.
