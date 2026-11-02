-- Embers trail the mouse, falling and cooling from yellow through orange to red as they go.
local HEAT = { { 255, 230, 140 }, { 255, 201, 74 }, { 245, 154, 42 }, { 232, 100, 26 }, { 194, 54, 15 }, { 138, 28, 12 } }
local GLYPHS = { "✦", "*", "+", "•", "·", "." }
local LIFE = 0.9
local sparks, last = {}, nil

local paint
paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  local p = ctx.pointer
  if p then
    if last and (last.x ~= p.x or last.y ~= p.y) then
      local steps = math.max(math.abs(p.x - last.x), math.abs(p.y - last.y))
      for i = 1, steps do
        local f = i / steps
        sparks[#sparks + 1] = {
          x = last.x + (p.x - last.x) * f,
          y = last.y + (p.y - last.y) * f,
          born = now - (1 - f) * 0.05,
          vx = (math.random() - 0.5) * 6,
        }
      end
    end
    last = { x = p.x, y = p.y }
  end
  local alive = {}
  for _, sp in ipairs(sparks) do
    local age = now - sp.born
    if age < LIFE then
      alive[#alive + 1] = sp
      local i = math.min(#HEAT, 1 + math.floor(age / LIFE * #HEAT))
      local x, y = math.floor(sp.x + sp.vx * age + 0.5), math.floor(sp.y + 5 * age * age + 0.5)
      s:set(x, y, GLYPHS[i], { fg = HEAT[i], bold = i <= 2 })
    end
  end
  sparks = alive
  if #sparks > 0 then paint:invalidate_after(33) end
end, { name = "trail", pointer = true })
