-- A confetti popper: two blasts from the bottom corners shoot up, slow in the air, then flutter down.
-- `/confetti` pops one, and so does a turn that kept you waiting more than 20 seconds.
local LONG_MS = 20000
local COLORS = { { 255, 90, 120 }, { 255, 200, 60 }, { 90, 200, 255 }, { 140, 230, 120 }, { 190, 130, 255 } }
-- Each bit tumbles between a face and an edge as it falls.
local SPIN = { { "■", "▬" }, { "◆", "◇" }, { "●", "•" }, { "▲", "▴" } }
local bits, last, pending, started, paint = {}, nil, false, nil, nil

--- One blast of `n` bits from `(x, y)`, aimed up and inward along `dir`.
local function blast(x, y, dir, n)
  for _ = 1, n do
    local speed = 70 + math.random() * 70
    local angle = math.rad(50 + math.random() * 35)
    bits[#bits + 1] = {
      x = x, y = y,
      -- Cells are about twice as tall as wide, so the climb is halved to look round.
      vx = dir * math.cos(angle) * speed, vy = -math.sin(angle) * speed / 2,
      p = math.random() * 6, spin = 4 + math.random() * 6,
      s = SPIN[math.random(#SPIN)], c = COLORS[math.random(#COLORS)],
    }
  end
end

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  local dt = math.min(0.05, last and now - last or 0)
  last = now
  local w, h = s:width(), s:height()
  if pending then
    pending = false
    blast(1, h - 2, 1, 70)
    blast(w - 2, h - 2, -1, 70)
  end
  local kept = {}
  for _, b in ipairs(bits) do
    -- Air slows the blast hard; then gravity, and a flutter once the bit is drifting down.
    local drag = math.exp(-1.6 * dt)
    b.vx, b.vy = b.vx * drag, (b.vy + 14 * dt) * drag
    local fall = 2.5 + (b.p % 1.5)
    if b.vy > fall then b.vy = fall end
    local flutter = b.vy > 0 and math.sin(now * 3 + b.p) * 2.5 or 0
    b.x, b.y = b.x + (b.vx + flutter) * dt, b.y + b.vy * dt
    local x, y = math.floor(b.x), math.floor(b.y)
    if y < h and x >= -2 and x < w + 2 then
      kept[#kept + 1] = b
      if x >= 0 and x < w and y >= 0 then
        local face = math.sin(now * b.spin + b.p) > -0.3
        s:set(x, y, b.s[face and 1 or 2], { fg = b.c })
      end
    end
  end
  bits = kept
  if #bits > 0 then paint:invalidate_after(16) end
end, { name = "confetti", order = 900 })

local function pop()
  pending = true
  paint:invalidate()
end
smelt.events.on("turn_start", function() started = smelt.time.monotonic_ms() end)
smelt.events.on("turn_end", function()
  if started and smelt.time.monotonic_ms() - started > LONG_MS then pop() end
  started = nil
end)
smelt.cmd.register("confetti", pop, { desc = "pop a confetti popper" })
