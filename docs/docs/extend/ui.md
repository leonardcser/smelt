---
title: "Windows, layouts and dialogs"
description: "Compose buffers, windows and layouts into overlays, dialogs and pickers for a smelt Lua plugin."
---

# Windows, layouts and dialogs

Build UI from three primitives:

| Primitive | Role |
| --------- | ---- |
| `smelt.buf` | Text, styled spans and marks |
| `smelt.win` | A buffer's view, input behavior and window options |
| `smelt.ui.layout` | Window arrangement, frames, padding and resizable splits |

Mount the layout in an overlay or dialog, or place it in the main screen.
Pickers add selection and filtering to these building blocks. All UI operations
here require the terminal interface. Blocking `open` calls must run in a
[coroutine](async.md#where-you-can-wait).

## Shared styles and spans

Buffer marks, styled text, window backgrounds, titles, bars and paint use the
same style value: a theme-group name or a `smelt.Style` table. A table can use
`hl_group` as a base and override individual attributes:

```lua
local style = { hl_group = "Comment", fg = "#88bbff", underline = true }
local buf = smelt.buf.new()
buf:styled({ { { text = "info", style = style } } })
local win = smelt.win.new(buf, { style = style, surface = "inert" })
```

Colors accept RGB (`{ r, g, b }` or `"#rrggbb"`), ANSI slots (`0` to `255`),
ANSI names (`"red"`, `"darkblue"`), theme-group names (`"SmeltAccent"`), or
`false` for the terminal default. RGB channels are rounded and clamped to
0-255. Colors read back from smelt use the same forms.

`smelt.style.merge(base, patch)` combines styles without changing the inputs or
resolving theme references; explicit `false` values remain intact.
`smelt.style.resolve(style)` returns a resolved snapshot. String window styles
follow theme changes; window style tables resolve when configured. Selection,
copy and range options belong to marks, not styles.

For colorschemes, see [Themes](../customize/appearance.md#themes). For all style
fields, see [`Style`](../reference/api/types.md#smeltstyle).

## Overlays

An overlay is a floating or docked window. Create a buffer, attach it to a
window, and mount the window in an overlay:

```lua
local buf = smelt.buf.new()
local win = smelt.win.new(buf, { surface = "inert" })

smelt.overlay.new({
  title     = { { text = " perf ", style = { bold = true } } },
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
error. All sizes, including integer cell counts, describe the complete view with
its chrome, just like overlay sizes.

### Replace presentation, keep behavior

`smelt.dialog.compose(body, opts)` frames the body layout. Override this function
to change presentation while keeping focus, keyboard handling and lifecycle:

```lua
local default = smelt.dialog.compose
smelt.dialog.compose = function(body, opts)
  opts = opts or {}
  return smelt.ui.layout.frame(body, {
    border = "rounded",
    title = opts.title,
    padding = 1,
  })
end
-- smelt.dialog.compose = default restores the bundled presentation.
```

This applies to future root dialogs, dialog pickers and the bundled centered
action modals. Keep every body window in the result so its behavior stays
connected; added windows join a root dialog's modal lifecycle. Return `body`
unchanged for a borderless dialog. Use `win:configure`, buffer styling and list
row renderers to style the leaves themselves. The helpers `dialog.input`,
`dialog.menu` and `dialog.content` are ordinary Lua functions too: they can be
wrapped to configure the windows they create without copying their behavior.

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

To restyle future pickers, wrap `smelt.picker.new` and configure the window
returned by `picker:win()`:

```lua
local new = smelt.picker.new
smelt.picker.new = function(opts)
  local picker = new(opts)
  picker:win():configure({ style = { bg = "#112233" }, pad_left = 2 })
  return picker
end
```

The existing controller still owns selection, filtering, async providers and
accept/dismiss handling. Do not rewrite its backing buffer: the native picker
materializes rows as its view scrolls.

For a different picker frontend, `smelt.dialog.picker` composes the existing
input, list and dialog primitives. Its `render(item)` callback accepts the same
`{ text, marks }` or `{ spans }` row format as `smelt.list.new`. Its `on_open(ctx)`
exposes `ctx.input`, `ctx.input_buf` and `ctx.list` for window customization.
It uses `smelt.dialog.compose` for chrome. Build from `smelt.input`,
`smelt.list`, windows and layouts when neither picker fits.

## Window events and marks

To decorate an existing buffer, subscribe to window events and draw marks in a
namespace you own:

```lua
local prompt = smelt.prompt.win
local ns     = smelt.ns("my_plugin")

prompt:on("text_changed", function()
  local buf = prompt:buf()
  if not buf then return end
  buf:clear_ns(ns):mark(ns, 1, 0, {
    end_col  = 999,
    style = "DiagnosticHint",
    priority = 200,
  })
end)
```

Event names and `mark` options are type-checked in your editor. Window options
are validated before any change is applied; unknown keys, invalid types and
unknown enum values raise at the call site.

## More UI building blocks

- [Bars and prompt layout](bars.md): statusline sources, prompt chrome and jump pills.
- [Paint and screen effects](paint.md): custom cell drawing, animation and pointer mapping.
- [Transcript rendering](transcript.md): semantic content, tool presentation and groups.
