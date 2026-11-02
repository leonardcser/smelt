-- /matrix: the matrix rains down the screen for a few seconds, then the last columns drain away.
local BURST, STEP = 6, 0.07
local GLYPHS = { "λ", "Σ", "Ξ", "Ж", "Я", "ψ", "Ω", "Ф", "0", "1", "7", "#", "$", "%", "&", "*", "+", "=", "<", ">" }
-- Falling columns by screen column, until when new ones start, and the clock the rain last stepped at.
local cols, until_s, stepped, paint = {}, 0, 0, nil

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  local w, h = s:width(), s:height()
  local raining = now < until_s
  if now - stepped >= STEP then
    stepped = now
    for x = 0, w - 1, 2 do
      local c = cols[x]
      if c then
        c.y = c.y + 1
        if c.y - c.len > h then cols[x] = nil end
      elseif raining and math.random() < 0.08 then
        cols[x] = { y = 0, len = math.random(5, 16) }
      end
    end
  end
  for x, c in pairs(cols) do
    for k = 0, c.len do
      local y = c.y - k
      if y >= 0 and y < h then
        local g = k == 0 and 255 or math.floor(200 * (1 - k / c.len))
        local glyph = GLYPHS[(x * 7 + y * 3 + c.len) % #GLYPHS + 1]
        s:set(x, y, glyph, { fg = { k == 0 and 220 or 0, g, k == 0 and 220 or 60 } })
      end
    end
  end
  if raining or next(cols) then paint:invalidate_after(33) end
end, { name = "matrix", order = 900 })

smelt.cmd.register("matrix", function()
  until_s = smelt.time.monotonic_ms() / 1000 + BURST
  paint:invalidate()
end, { desc = "the matrix, for a few seconds" })
