-- The theme's colors drift slowly round the color wheel, and jump ahead on every turn.
local hue = 0
local function apply()
  local c, s = math.cos(math.rad(hue)), math.sin(math.rad(hue))
  local m = {
    0.299 + 0.701 * c + 0.168 * s, 0.587 - 0.587 * c + 0.330 * s, 0.114 - 0.114 * c - 0.497 * s,
    0.299 - 0.299 * c - 0.328 * s, 0.587 + 0.413 * c + 0.035 * s, 0.114 - 0.114 * c + 0.292 * s,
    0.299 - 0.300 * c + 1.250 * s, 0.587 - 0.588 * c - 1.050 * s, 0.114 + 0.886 * c - 0.203 * s,
  }
  local function rotate(rgb)
    local out = {}
    for i = 0, 2 do
      out[i + 1] = m[i * 3 + 1] * rgb[1] + m[i * 3 + 2] * rgb[2] + m[i * 3 + 3] * rgb[3]
    end
    return out
  end
  local spec = smelt.theme.spec("Dracula")
  spec.name = "drift"
  for _, s in pairs(spec.groups) do
    if type(s) == "table" then
      if type(s.fg) == "table" then s.fg = rotate(s.fg) end
      if type(s.bg) == "table" then s.bg = rotate(s.bg) end
    end
  end
  spec.groups.Normal = { fg = spec.groups.Normal.fg, bg = rotate({ 40, 42, 54 }) }
  smelt.theme.apply(spec)
end
smelt.events.on("turn_end", function() hue = hue + 60 end)
smelt.timer.every(120, function()
  hue = (hue + 2) % 360
  apply()
end)
