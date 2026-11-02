-- Gravity: `/clear` and `/new` let go of the conversation, and its letters fall off the screen.
local fx = require("fx")

local G = 90
-- `armed` waits for the conversation to go; `falling` holds its letters once it has.
local armed, falling, paint = nil, nil, nil

--- The letters inside `view` on `snap`, the lowest rows letting go first and each a little on its own.
local function letters(snap, view)
  local out, bottom = {}, view.y + view.height
  for y = view.y, bottom - 1 do
    for x = view.x, view.x + view.width - 1 do
      local c = snap:get(x, y)
      if c and c.width == 1 and c.ch ~= " " then
        out[#out + 1] = {
          x = x, y = y, ch = c.ch, style = c.style,
          delay = (bottom - y) / view.height * 0.35 + math.random() * 0.12,
          vx = (math.random() - 0.5) * 6,
        }
      end
    end
  end
  return out
end

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  if armed then
    local gone = true
    for _, b in ipairs(fx.blocks()) do
      if armed.ids[b.block_id] then gone = false end
    end
    if gone and ctx.prev then
      falling = { view = armed.view, start = now, letters = letters(ctx.prev, armed.view) }
      armed = nil
    elseif now - armed.at > 1 then
      armed = nil
    end
  end
  if not falling then return end
  local t, r, alive = now - falling.start, falling.view, false
  for _, p in ipairs(falling.letters) do
    local dt = math.max(0, t - p.delay)
    local x = math.floor(p.x + p.vx * dt + 0.5)
    local y = math.floor(p.y + 0.5 * G * dt * dt + 0.5)
    if y < r.y + r.height then
      alive = true
      if x >= r.x and x < r.x + r.width then s:set(x, y, p.ch, p.style) end
    end
  end
  if alive then
    paint:invalidate_after(16)
  else
    falling = nil
    paint:invalidate()
  end
end, { name = "gravity", history = true })

smelt.events.on("cmd_pre", function(name)
  if name ~= "clear" and name ~= "new" then return end
  local ids = {}
  for _, b in ipairs(fx.blocks()) do ids[b.block_id] = true end
  if next(ids) == nil then return end
  armed = { ids = ids, view = smelt.transcript.win:rect(), at = fx.now() }
  paint:invalidate()
end)
