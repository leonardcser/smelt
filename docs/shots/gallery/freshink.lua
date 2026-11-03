-- Fresh ink: the text the model writes lands bright and dries into its own color.
local fx = require("smelt.fx")

local LIFE = 1.2
local INK = { 255, 236, 170 }
local fresh = fx.fresh(LIFE)
local paint

paint = smelt.paint.frame(function(s, ctx)
  local wet = fresh(s, ctx)
  for _, g in ipairs(wet) do
    local k = 1 - (ctx.now_ms / 1000 - g.t) / LIFE
    local fg = fx.rgb(s:get(g.x, g.y).style.fg, "fg")
    s:patch_style(g.x, g.y, 1, 1, { fg = fx.mix(fg, INK, k * k), bold = k > 0.6 })
  end
  if #wet > 0 then paint:invalidate_after(33) end
end, { name = "freshink", history = true })
