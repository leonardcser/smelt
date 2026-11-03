-- Decode: the text the model writes arrives scrambled and green, and resolves glyph by glyph.
local fx = require("smelt.fx")

local SETTLE = 0.45
local FLASH = 0.25
local GLYPHS = {}
for c in ("01<>[]{}/\\|=+*#%$&@?!;:abcdefxyz"):gmatch(".") do GLYPHS[#GLYPHS + 1] = c end
local CODE, DEEP, LIGHT = { 90, 255, 120 }, { 20, 90, 40 }, { 225, 255, 230 }
local fresh = fx.fresh(0.08 + SETTLE + FLASH)
local paint

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  local busy = fresh(s, ctx)
  for _, g in ipairs(busy) do
    local seed = g.y * 4096 + g.x + g.t
    local settle = g.t + 0.08 + fx.hash(seed) * SETTLE
    if now < settle then
      local fade = (settle - now) / SETTLE
      local ch = GLYPHS[1 + math.floor(fx.hash(seed + ctx.frame * 7.31) * #GLYPHS)]
      s:set(g.x, g.y, ch, { fg = fx.mix(CODE, DEEP, fade * 0.6), bold = fade < 0.3 })
    elseif now < settle + FLASH then
      local fg = fx.rgb(s:get(g.x, g.y).style.fg, "fg")
      s:patch_style(g.x, g.y, 1, 1, { fg = fx.mix(fg, LIGHT, (settle + FLASH - now) / FLASH) })
    end
  end
  if #busy > 0 then paint:invalidate_after(33) end
end, { name = "decode", history = true })
