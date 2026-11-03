-- A harmonograph screensaver: two damped pendulums draw a curve in fine braille lines, its color
-- drifting through the spectrum as it goes, then a new figure starts. It comes on after two minutes
-- without input, or with /screensaver, and any key puts it away. One quiet line says what the agent
-- is doing, and how long the turn took once it is done.
local fx = require("smelt.fx")
local IDLE_MS = 2 * 60 * 1000
-- Seconds a figure takes to draw, and how long it stays before the next.
local DRAW, HOLD = 9, 2.5
local BG, DIM = { 10, 10, 16 }, { 120, 120, 140 }
-- Braille dot bits by column and row inside a cell.
local BITS = { { 0x01, 0x02, 0x04, 0x40 }, { 0x08, 0x10, 0x20, 0x80 } }

--- A color at hue `h` (turns), bright and saturated.
local function hue(h)
  local function channel(n)
    local k = (n + h * 6) % 6
    return 70 + 185 * (1 - math.max(0, math.min(k, 4 - k, 1)))
  end
  return { channel(5), channel(3), channel(1) }
end

-- The figure being drawn: its pendulums, when it started, how far it got, and the dots so far by
-- cell, each with its color.
local fig, cells = nil, {}

--- A new figure: four pendulums near small whole frequencies, detuned a little so the curve
--- precesses; the two across never match the two down, or the figure is a line.
local function figure(now)
  local function pendulum(f)
    return { f = f + (math.random() - 0.5) * 0.03, p = math.random() * math.pi * 2, d = 0.008 + math.random() * 0.008 }
  end
  local a, b = math.random(1, 4), math.random(1, 4)
  local c = a == b and a % 4 + 1 or math.random(1, 4)
  fig = {
    x = { pendulum(a), pendulum(b) }, y = { pendulum(c), pendulum(b) },
    at = now, t = 0, h = math.random(),
  }
  cells = {}
end

local function swing(ps, t)
  local v = 0
  for _, p in ipairs(ps) do v = v + math.sin(p.f * t + p.p) * math.exp(-p.d * t) end
  return v / #ps
end

-- When the turn started, how long it took once it finished behind the screensaver, and the last input.
local started, done_in, close, last_input, paint = nil, nil, nil, smelt.time.monotonic_ms(), nil

local function clock(secs) return string.format("%d:%02d", secs // 60, secs % 60) end

local function status()
  if started then return "● working  " .. clock((smelt.time.monotonic_ms() - started) // 1000), { 255, 170, 70 } end
  if done_in then return "✓ done in " .. clock(done_in), { 150, 220, 120 } end
  return os.date("%H:%M"), DIM
end

paint = smelt.paint.register(function(s)
  local w, h = s:width(), s:height()
  local now = smelt.time.monotonic_ms() / 1000
  if not fig or now - fig.at > DRAW + HOLD then figure(now) end
  -- Dots are two across and four down a cell, so the figure is square on screen.
  local dw, dh = w * 2, h * 4
  local size = math.min(dw, dh - 8) * 0.47
  local target = math.min(1, (now - fig.at) / DRAW) * 160
  while fig.t < target do
    fig.t = fig.t + 0.02
    local px = math.floor(dw / 2 + swing(fig.x, fig.t) * size)
    local py = math.floor((dh - 8) / 2 + swing(fig.y, fig.t) * size)
    local cx, cy = px // 2, py // 4
    if cx >= 0 and cx < w and cy >= 0 and cy < h - 2 then
      local key = cy * w + cx
      local c = cells[key] or { 0 }
      c[1] = c[1] | BITS[px % 2 + 1][py % 4 + 1]
      c[2] = hue(fig.h + fig.t / 160)
      cells[key] = c
    end
  end
  s:fill(0, 0, w, h, " ", { bg = BG })
  for key, c in pairs(cells) do
    s:set(key % w, key // w, utf8.char(0x2800 + c[1]), { fg = c[2], bg = BG })
  end
  local text, color = status()
  s:put_str((w - smelt.text.width(text)) // 2, h - 1, text, { fg = color, bg = BG })
  paint:invalidate_after(50)
end)

local function open()
  if close then return end
  done_in, fig = nil, nil
  close = fx.cover(paint, function()
    close = nil
    last_input = smelt.time.monotonic_ms()
  end)
end

local function touched() last_input = smelt.time.monotonic_ms() end
smelt.prompt.win:on("text_changed", touched)
smelt.events.on("input_submit", touched)
smelt.events.on("cmd_pre", touched)
smelt.events.on("turn_start", function() started = smelt.time.monotonic_ms() end)
smelt.events.on("turn_end", function()
  if close and started then done_in = (smelt.time.monotonic_ms() - started) // 1000 end
  started = nil
end)
smelt.timer.every(5000, function()
  if smelt.time.monotonic_ms() - last_input > IDLE_MS then open() end
end)
smelt.cmd.register("screensaver", open, { desc = "a harmonograph until you press a key" })
