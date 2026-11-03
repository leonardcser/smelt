-- Barrel roll: `/barrelroll` spins the whole screen one full turn, pulling back as it goes so it
-- spins clear of the edges. Letters stay upright; lines, corners and half blocks turn with it.
local fx = require("smelt.fx")

local SPIN = 1.4
local start, paint = nil, nil

--- Where the spin is now: the angle, and the factor it pulls back by.
local function pose()
  local p = math.min(1, (fx.now() - start) / SPIN)
  return 2 * math.pi * fx.ease(p), 1 - 0.4 * math.sin(math.pi * p), p
end

--- The cell of a `w` by `h` screen that shows at `(x, y)` turned by `a` and pulled back by `zoom`.
--- Rows are twice as tall as columns are wide, so the turn happens in square units.
local function source(x, y, w, h, a, zoom)
  local cx, cy = (w - 1) / 2, (h - 1) / 2
  local cos, sin = math.cos(a) / zoom, math.sin(a) / zoom
  local px, py = x - cx, (y - cy) * 2
  return math.floor(cx + px * cos + py * sin + 0.5), math.floor(cy + (py * cos - px * sin) / 2 + 0.5)
end

local function frame(s)
  if not start then return end
  local a, zoom, p = pose()
  local snap, w, h = s:snapshot(), s:width(), s:height()
  -- The glyph turn nearest the angle: none, a quarter clockwise, a half, a quarter back.
  local glyphs = ({ [1] = fx.QUARTER, [2] = fx.TURN, [3] = fx.QUARTER_BACK })[math.floor(a / (math.pi / 2) + 0.5) % 4]
  local blank = { bg = fx.void() }
  for y = 0, h - 1 do
    for x = 0, w - 1 do
      local sx, sy = source(x, y, w, h, a, zoom)
      local c = snap:get(sx, sy)
      if not c then
        s:set(x, y, " ", blank)
      else
        -- A wide glyph can't land on two cells at once mid-turn, so it shows as a dot.
        local ch = c.width == 2 and "•" or c.width == 0 and " " or c.ch
        s:set(x, y, glyphs and glyphs[ch] or ch, c.style)
      end
    end
  end
  if p >= 1 then
    start = nil
    paint:invalidate()
  else
    paint:invalidate_after(16)
  end
end

--- The paint is made on the first roll, since it needs the running app.
local function ensure_paint()
  paint = paint or smelt.paint.frame(frame, {
    name = "barrelroll",
    order = 100,
    map_pointer = function(x, y, w, h)
      if not start then return nil end
      local a, zoom = pose()
      return source(x, y, w, h, a, zoom)
    end,
  })
end

smelt.cmd.register("barrelroll", function()
  if start then return end
  ensure_paint()
  start = fx.now()
  paint:invalidate()
end, { desc = "spin the screen one full turn" })
