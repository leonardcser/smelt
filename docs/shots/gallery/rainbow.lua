-- /rainbow: a rainbow drifts across all the text on screen until you run it again.
local on = false
local paint

local function rainbow(i)
  return {
    math.sin(i) * 110 + 145,
    math.sin(i + 2.094) * 110 + 145,
    math.sin(i + 4.189) * 110 + 145,
  }
end

paint = smelt.paint.frame(function(s, ctx)
  if not on then return end
  local t = ctx.now_ms / 1000
  s:map(function(cell, x, y)
    if cell.ch == " " or cell.ch == "" then return nil end
    local st = cell.style
    st.fg = rainbow(x * 0.09 + y * 0.18 - t * 1.2)
    return { style = st }
  end)
  paint:invalidate_after(50)
end, { name = "rainbow" })

smelt.cmd.register("rainbow", function()
  on = not on
  paint:invalidate()
end, { desc = "rainbow text" })
