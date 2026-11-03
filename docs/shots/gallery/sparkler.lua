-- Sparkler: sparks fly from wherever the model is writing, arcing down and cooling as they fall.
local fx = require("smelt.fx")

local HEAT = { { 255, 250, 210 }, { 255, 215, 120 }, { 255, 160, 60 }, { 230, 90, 30 }, { 150, 40, 20 } }
local GLYPHS = { "✦", "*", "+", "·", "." }
local LIFE = 0.7
local fresh = fx.fresh(0.15)
local sparks, paint = {}, nil

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  -- The pen: the newest glyph, the lowest and then the rightmost of those written together.
  local pen
  for _, g in ipairs(fresh(s, ctx)) do
    if not pen or g.t > pen.t or (g.t == pen.t and (g.y > pen.y or (g.y == pen.y and g.x > pen.x))) then pen = g end
  end
  if pen and now - pen.t < 0.1 then
    for _ = 1, 3 do
      sparks[#sparks + 1] = { x = pen.x + 1, y = pen.y, vx = (math.random() - 0.3) * 14, vy = -math.random() * 7, t = now }
    end
  end
  local kept = {}
  for _, sp in ipairs(sparks) do
    local age = now - sp.t
    if age < LIFE then
      kept[#kept + 1] = sp
      local x = math.floor(sp.x + sp.vx * age + 0.5)
      local y = math.floor(sp.y + sp.vy * age + 9 * age * age + 0.5)
      local i = math.min(#HEAT, 1 + math.floor(age / LIFE * #HEAT))
      local c = s:get(x, y)
      if c and c.ch == " " then s:set(x, y, GLYPHS[i], { fg = HEAT[i], bold = i <= 2 }) end
    end
  end
  sparks = kept
  if #sparks > 0 then paint:invalidate_after(33) end
end, { name = "sparkler", history = true })
