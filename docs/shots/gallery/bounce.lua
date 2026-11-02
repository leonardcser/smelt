-- /dvd: a small DVD logo bounces over the conversation like an idle player, changing color at every
-- wall, until you run it again.
local LOGO = {
  "█▀▄ █ █ █▀▄",
  "█▄▀ ▀▄▀ █▄▀",
}
local LW, LH, STEP = 11, 3, 0.1
local COLORS = { { 255, 110, 90 }, { 90, 200, 255 }, { 170, 120, 255 }, { 120, 230, 130 }, { 255, 210, 80 }, { 255, 120, 220 } }
local on, x, y, dx, dy, c, stepped, paint = false, 3, 2, 1, 1, 1, 0, nil

paint = smelt.paint.frame(function(s, ctx)
  if not on then return end
  local w, h = s:width(), s:height()
  local now = ctx.now_ms / 1000
  if now - stepped >= STEP then
    stepped = now
    x, y = x + dx, y + dy
    if x <= 0 or x >= w - LW then dx, c = -dx, c % #COLORS + 1 end
    if y <= 0 or y >= h - LH then dy, c = -dy, c % #COLORS + 1 end
    x, y = math.max(0, math.min(w - LW, x)), math.max(0, math.min(h - LH, y))
  end
  local color = COLORS[c]
  for row, line in ipairs(LOGO) do
    local col = 0
    for ch in line:gmatch(utf8.charpattern) do
      if ch ~= " " then s:set(x + col, y + row - 1, ch, { fg = color }) end
      col = col + 1
    end
  end
  -- The disc under the letters, with VIDEO cut out of it.
  s:put_str(x + 1, y + 2, "◖", { fg = color })
  s:put_str(x + 2, y + 2, " VIDEO ", { fg = { 10, 10, 14 }, bg = color, bold = true })
  s:put_str(x + 9, y + 2, "◗", { fg = color })
  paint:invalidate_after(math.floor(STEP * 1000))
end, { name = "dvd", order = 900 })

smelt.cmd.register("dvd", function()
  on = not on
  paint:invalidate()
end, { desc = "a DVD logo bouncing over the conversation" })
