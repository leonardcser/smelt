-- /flashlight: lights out. The whole screen goes dark but for a flickering beam that follows the
-- mouse, until you run it again.
local RADIUS, DARK, WARM = 15, { 4, 2, 8 }, { 1, 0.93, 0.75 }

--- `c` (any color) lit by `k`, from the dark at 0 to its own warm-tinted color at 1.
local function lit(c, layer, k)
  local v = smelt.theme.rgb(c, layer)
  return {
    DARK[1] + (v[1] * WARM[1] - DARK[1]) * k,
    DARK[2] + (v[2] * WARM[2] - DARK[2]) * k,
    DARK[3] + (v[3] * WARM[3] - DARK[3]) * k,
  }
end

local on, paint = false, nil
paint = smelt.paint.frame(function(s, ctx)
  if not on then return end
  local p = ctx.pointer or ctx.cursor or { x = s:width() // 2, y = s:height() // 2 }
  local t = ctx.now_ms / 1000
  local flicker = 0.9 + 0.1 * math.sin(t * 31) * math.sin(t * 7.3)
  if math.sin(t * 2.1) * math.sin(t * 5.7) > 0.93 then flicker = 0.35 end
  s:map(function(cell, x, y)
    -- Cells are about twice as tall as they are wide, so the beam is round on screen.
    local dx, dy = (x - p.x) / 2, y - p.y
    local d = math.sqrt(dx * dx + dy * dy) / RADIUS * 2
    local k = math.min(1, math.max(0, (1 - d) * 1.8)) * flicker
    local style = cell.style
    style.fg = lit(style.fg, "fg", k)
    style.bg = lit(style.bg, "bg", k)
    return { style = style }
  end)
  paint:invalidate_after(33)
end, { name = "halloween.flashlight", pointer = true, order = 950 })

smelt.cmd.register("flashlight", function()
  on = not on
  paint:invalidate()
end, { desc = "lights out but for a flashlight" })
