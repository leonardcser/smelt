-- Away: when the terminal loses focus, smelt fades to a dim gray, the way a dimmed window shows it is
-- in the background, and its colors come back as soon as you return.
local fx = require("smelt.fx")

local OUT, BACK = 0.9, 0.25
local DIM = 0.45
-- How far faded, from 0 to 1, and the clock it was last stepped at.
local level, last, paint = 0, nil, nil

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  -- Idle frames are far apart; a fade steps from the first frame after a change.
  local dt = math.min(0.05, last and now - last or 0)
  last = now
  local want = ctx.focused and 0 or 1
  if level ~= want then
    local step = dt / (want > level and OUT or BACK)
    level = want > level and math.min(want, level + step) or math.max(want, level - step)
  end
  if level ~= want then paint:invalidate_after(16) end
  if level == 0 then return end
  local k = fx.ease(level)
  local bg = fx.rgb(nil, "bg")
  s:recolor(function(r, g, b, layer)
    local gray = 0.2126 * r + 0.7152 * g + 0.0722 * b
    local c = fx.mix({ r, g, b }, { gray, gray, gray }, k)
    if layer == "fg" then c = fx.mix(c, bg, DIM * k) end
    return c[1], c[2], c[3]
  end, { defaults = true })
end, { name = "away", order = 980 })
