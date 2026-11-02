-- Night shift: the screen warms after dark, the way a night light does, and cools back by morning.
-- `/nightshift` steps between following the clock, always on, and off.
local MODES = { "auto", "on", "off" }
local mode = "auto"
local paint

--- How warm the screen is at `hour` (0-24): none by day, ramping in 19-22h and out 6-8h.
local function by_clock(hour)
  if hour >= 22 or hour < 6 then return 1 end
  if hour >= 19 then return (hour - 19) / 3 end
  if hour < 8 then return 1 - (hour - 6) / 2 end
  return 0
end

local function strength()
  if mode == "on" then return 1 end
  if mode == "off" then return 0 end
  local t = os.date("*t")
  return by_clock(t.hour + t.min / 60)
end

paint = smelt.paint.frame(function(s)
  local k = strength()
  if k == 0 then return end
  local g, b = 1 - 0.2 * k, 1 - 0.55 * k
  -- The terminal's own colors too: they are most of the text and the background.
  s:recolor(function(r, gg, bb) return r, gg * g, bb * b end, { defaults = true })
end, { name = "nightshift", order = 900 })

-- The clock moves the strength between frames.
smelt.timer.every(60000, function() paint:invalidate() end)

smelt.cmd.register("nightshift", function(arg)
  if arg and arg ~= "" then
    mode = arg:lower()
  else
    for i, m in ipairs(MODES) do
      if m == mode then mode = MODES[i % #MODES + 1] break end
    end
  end
  paint:invalidate()
  smelt.notify.info("night shift: " .. mode)
end, { desc = "warm the screen at night", args = MODES })
