-- Bloom: vivid colored text glows, its light spilling softly onto the background around it.
local fx = require("smelt.fx")

-- Mostly sideways: a cell is twice as tall as it is wide, so light spilling up and down reads as blocks.
local KERNEL = {
  { 0, 0, 0.26 },
  { -1, 0, 0.18 }, { 1, 0, 0.18 }, { -2, 0, 0.08 }, { 2, 0, 0.08 }, { -3, 0, 0.03 }, { 3, 0, 0.03 },
  { 0, -1, 0.04 }, { 0, 1, 0.04 },
}
local STRENGTH, CAP = 0.45, 55

smelt.paint.frame(function(s)
  local w = s:width()
  local glow = {}
  s:map(function(cell, x, y)
    if cell.ch == " " or cell.ch == "" then return nil end
    local c = fx.rgb(cell.style.fg, "fg")
    local hi, lo = math.max(c[1], c[2], c[3]), math.min(c[1], c[2], c[3])
    if hi - lo < 80 or hi < 150 then return nil end
    local k = STRENGTH * (cell.style.bold and 1.3 or 1) * (hi - lo) / 255
    for _, kern in ipairs(KERNEL) do
      -- Light spilling past the edge lands nowhere: the second pass never reads it.
      local i = (y + kern[2]) * w + x + kern[1] + 1
      local g = glow[i]
      if not g then
        g = { 0, 0, 0 }
        glow[i] = g
      end
      local f = k * kern[3]
      g[1], g[2], g[3] = g[1] + c[1] * f, g[2] + c[2] * f, g[3] + c[3] * f
    end
  end)
  if next(glow) == nil then return end
  s:map(function(cell, x, y)
    local g = glow[y * w + x + 1]
    if not g then return nil end
    local b = fx.rgb(cell.style.bg, "bg")
    local st = cell.style
    st.bg = { b[1] + math.min(CAP, g[1]), b[2] + math.min(CAP, g[2]), b[3] + math.min(CAP, g[3]) }
    return { style = st }
  end)
end, { name = "bloom" })
