-- Flip: `/flip` turns the whole screen over like a card, around its middle row, and smelt keeps
-- working upside down until the next `/flip`: typing, clicks and scrolling land where they show. On
-- April 1 it turns over by itself at startup.
local fx = require("fx")

local TURN = 0.9
local flipped, anim, paint = false, nil, nil

--- `style` with its colors darkened toward `void` by `k` (1 keeps them). The terminal's own
--- background stays its own.
local function shade(style, k, void)
  if k < 1 then
    style.fg = fx.mix(void, fx.rgb(style.fg, "fg"), k)
    if style.bg then style.bg = fx.mix(void, fx.rgb(style.bg, "bg"), k) end
  end
  return style
end

paint = smelt.paint.frame(function(s, ctx)
  if not flipped and not anim then return end
  local snap, w, h = s:snapshot(), s:width(), s:height()
  local p = anim and math.min(1, (fx.now() - anim.start) / TURN) or 1
  -- How far over the card is: 0 face up, pi face down.
  local a = anim and math.pi * (anim.over and fx.ease(p) or 1 - fx.ease(p)) or math.pi
  local down, scale = a > math.pi / 2, math.abs(math.cos(a))
  local void = fx.void()
  local mid = (h - 1) / 2
  -- Edge on, the card is darkest; the rows nearest you a little brighter than the far ones.
  local light = 0.3 + 0.7 * scale
  for y = 0, h - 1 do
    local from = math.floor(mid + (y - mid) / math.max(scale, 1e-3) + 0.5)
    if from < 0 or from >= h then
      s:fill(0, y, w, 1, " ", { bg = void })
    else
      local tilt = (y - mid) / h * math.sin(a) * (down and -1 or 1)
      local k = anim and math.max(0.15, math.min(1, light + 0.35 * tilt)) or 1
      local src = down and h - 1 - from or from
      for x = 0, w - 1 do
        local c = snap:get(x, src)
        if c.width > 0 then
          -- Face down, each row reads right to left, and a wide glyph keeps its two cells in order.
          local at = down and w - x - c.width or x
          s:set(at, y, down and fx.TURN[c.ch] or c.ch, shade(c.style, k, void))
        end
      end
    end
  end
  if not anim and ctx.cursor then s:set_cursor(w - 1 - ctx.cursor.x, h - 1 - ctx.cursor.y) end
  if p >= 1 and anim then
    flipped, anim = anim.over, nil
    paint:invalidate()
  elseif anim then
    paint:invalidate_after(16)
  end
end, {
  name = "flip",
  -- Over the other effects, so they turn over with the screen.
  order = 100,
  map_pointer = function(x, y, w, h)
    if flipped and not anim then return w - 1 - x, h - 1 - y end
  end,
})

local function turn_over()
  if anim then return end
  anim = { over = not flipped, start = fx.now() }
  paint:invalidate()
end

smelt.cmd.register("flip", turn_over, { desc = "turn the screen upside down, or back" })

if os.date("%m-%d") == "04-01" then smelt.timer.after(1200, turn_over) end
