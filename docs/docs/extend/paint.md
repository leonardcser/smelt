---
title: "Paint and screen effects"
description: "Draw custom cells, animate views and apply screen effects from a smelt Lua plugin."
---

# Paint and screen effects

Use paint when buffers and styled text are not enough. Both paint APIs call
`fn(slice, ctx)`, with coordinates in cells from the top-left:

- `smelt.paint.register(fn, opts)` creates a leaf for a
  [layout or overlay](ui.md). It draws only its assigned rectangle.
- `smelt.paint.frame(fn, opts)` processes the finished screen. Its effect handle
  supports `remove`, `invalidate` and `invalidate_after`, but is not a layout leaf.

These APIs require the terminal UI. See the [paint reference](../reference/api/paint.md)
for signatures and option types.

## Draw cells

The slice contains what was drawn below the paint. Read it with `get`, write
with `set` or `put_str`, and fill rectangles with `fill`. Out-of-bounds writes
are ignored; reads return `nil`.

```lua
local label = smelt.paint.register(function(slice)
  slice:put_str(0, 0, "ready", "SmeltAccent")
end, { name = "myplugin.label" })

smelt.overlay.new({
  layout = smelt.ui.layout.leaf(label, { measure = { 5, 1 } }),
})
```

A slice is valid only during its callback. Use `snapshot()` to retain cells and
`blit(snapshot, x, y)` to draw them later. See
[`Slice`](../reference/api/types.md#smeltpaintslice) for all drawing operations.

Styles use the [shared style and color formats](ui.md#shared-styles-and-spans).
There are two write behaviors:

- `set`, `put_str` and `fill` replace text attributes. An omitted background
  preserves the cell's background; `bg = false` selects the terminal default.
- `patch_style` and styles returned from `map` preserve omitted fields. Use
  explicit `false` to clear an attribute or select a default color.

## Recolor the screen

`recolor` calls a function once per distinct color, with RGB channels in 0-255
and a layer of `"fg"` or `"bg"`. Return new channels, or `nil` to keep the color:

```lua
local warm = smelt.paint.frame(function(slice)
  slice:recolor(function(r, g, b) return r, g * 0.85, b * 0.6 end)
end, { name = "myplugin.nightshift" })

-- warm:remove() stops the effect.
```

Terminal-default colors stay unchanged unless you pass `{ defaults = true }`
to `recolor`. Replacing a default background with an explicit color makes it
opaque on a translucent terminal.

For blending, `smelt.theme.rgb(color, layer)` resolves a color or theme group
to `{ r, g, b }`; `nil` resolves the terminal default for that layer. smelt asks
the terminal for its default and ANSI colors at startup and falls back to the
xterm palette if it gets no answer.

Frame effects run in ascending `order` (default 0), then registration order.
Give a filter that must see earlier effects a higher order.

## Animate only while needed

Changes under a paint trigger a repaint. If plugin state changes, call
`handle:invalidate()`. For animation, request the next frame from the callback:

```lua
local fx
fx = smelt.paint.frame(function(slice, ctx)
  -- Draw using ctx.now_ms.
  if still_moving then fx:invalidate_after(16) end
end)
```

Stop requesting frames to go idle. Leaves that are off screen stop painting.
Use `ctx.now_ms` or `smelt.time.monotonic_ms()` for animation, not wall time
(`smelt.time.now_ms()`). The bundled `/donut` plugin is a complete example.

[`Ctx`](../reference/api/types.md#smeltpaintctx) also exposes the slice's screen
position, terminal size, frame number and focus state. For frame effects,
`focused` means terminal focus and `cursor` is the visible caret position.

## React to changed content

Pass `history = true` to a frame effect to receive:

- `ctx.prev`: the previous screen before frame effects, absent on the first frame.
- `ctx.scrolls`: text-scroll regions as `{ win, x, y, width, height, dy }`.
  Positive `dy` moves content up; negative moves it down. Within an unobscured
  region, row `y` corresponds to the previous row `y + dy`.
- `ctx.damage`: changed cells as one-row `{ x, y, width, height }` runs.
  Scroll-only moves are excluded. The first frame and a resize damage all rows.

Match these regions against `smelt.transcript.visible_blocks()` to distinguish
new model text from other changes. Each visible block has a screen `rect`, its
first visible block row in `top`, and a `streaming` flag:

```lua
-- Fresh ink: highlight newly streamed text.
smelt.paint.frame(function(slice, ctx)
  for _, block in ipairs(smelt.transcript.visible_blocks()) do
    if block.streaming and block.rect then
      for _, d in ipairs(ctx.damage) do
        if d.y >= block.rect.y and d.y < block.rect.y + block.rect.height then
          slice:patch_style(d.x, d.y, d.width, 1, {
            fg = { 255, 236, 170 }, bold = true,
          })
        end
      end
    end
  end
end, { name = "myplugin.freshink", history = true })
```

## Handle the pointer

Paint leaves and windows use `handle:on(event, fn)`. Coordinates are relative
to the leaf:

- `press`, `release`, `drag` include `button`. The pressed leaf retains drag
  and release events even after the pointer leaves it.
- `enter`, `leave`, `move` report hover changes with no button held.

```lua
local hot = false
local button = smelt.paint.register(function(slice)
  slice:put_str(0, 0, " run ", { reverse = hot })
end, { name = "myplugin.button" })
button:on("enter", function() hot = true end)
button:on("leave", function() hot = false end)
button:on("press", function(ev) smelt.notify.info("clicked at " .. ev.x) end)
```

Pass `pointer = true` to repaint on pointer movement and receive
`ctx.pointer = { x, y }`. For a leaf it is slice-relative; for a frame effect
it stays in screen coordinates.

An effect that moves cells must also map input. Supply
`map_pointer(x, y, width, height)` to translate the displayed cell back to its
original screen cell. Effects map in reverse drawing order. For a vertical
flip, the mapping is:

```lua
map_pointer = function(x, y, _, height) return x, height - 1 - y end
```

The visible caret moves with the cells. Also call `slice:set_cursor(x, y)` at
its transformed position so input-method text follows it.
