-- Color vision: `/colorblind <protan|deutan|tritan|mono>` shows smelt as someone with that color
-- vision sees it, to check a theme; `/colorblind fix <protan|deutan|tritan>` shifts the colors they
-- can't tell apart into ones they can; `/colorblind off` turns it off.

-- Machado, Oliveira and Fernandes (2009), full severity, on linear rgb.
local SEE = {
  protan = { { 0.152286, 1.052583, -0.204868 }, { 0.114503, 0.786281, 0.099216 }, { -0.003882, -0.048116, 1.051998 } },
  deutan = { { 0.367322, 0.860646, -0.227968 }, { 0.280085, 0.672501, 0.047413 }, { -0.011820, 0.042940, 0.968881 } },
  tritan = { { 1.255528, -0.076749, -0.178779 }, { -0.078411, 0.930809, 0.147602 }, { 0.004733, 0.691367, 0.303900 } },
  mono = { { 0.2126, 0.7152, 0.0722 }, { 0.2126, 0.7152, 0.0722 }, { 0.2126, 0.7152, 0.0722 } },
}

local vision, fix = nil, false

local function linear(v)
  v = v / 255
  return v <= 0.04045 and v / 12.92 or ((v + 0.055) / 1.055) ^ 2.4
end

local function srgb(v)
  v = math.max(0, math.min(1, v))
  return 255 * (v <= 0.0031308 and v * 12.92 or 1.055 * v ^ (1 / 2.4) - 0.055)
end

local function seen(m, c)
  local l = { linear(c[1]), linear(c[2]), linear(c[3]) }
  local out = {}
  for i = 1, 3 do
    out[i] = srgb(m[i][1] * l[1] + m[i][2] * l[2] + m[i][3] * l[3])
  end
  return out
end

local function convert(r, g, b)
  local s = seen(SEE[vision], { r, g, b })
  if not fix then return s[1], s[2], s[3] end
  -- Daltonize: what was lost moves onto the channels still seen.
  local e = { r - s[1], g - s[2], b - s[3] }
  if vision == "tritan" then return r + 0.7 * e[3], g + 0.7 * e[3], b end
  return r, g + 0.7 * e[1], b + 0.7 * e[1]
end

local paint = smelt.paint.frame(function(s)
  if vision then s:recolor(convert, { defaults = true }) end
end, {
  name = "colorblind",
  -- Last of all, so it sees every other effect the way the eye would.
  order = 1000,
})

smelt.cmd.register("colorblind", function(arg)
  local words = {}
  for w in (arg or ""):lower():gmatch("%S+") do words[#words + 1] = w end
  local f = words[1] == "fix"
  local v = words[f and 2 or 1]
  if v == nil or v == "off" then
    vision = nil
    paint:invalidate()
    smelt.notify.info("color vision: normal")
    return
  end
  if not SEE[v] or (f and v == "mono") then
    smelt.notify.error("usage: /colorblind [fix] <protan|deutan|tritan|mono> | off")
    return
  end
  vision, fix = v, f
  paint:invalidate()
  smelt.notify.info((f and "corrected for " or "seen with ") .. v)
end, { desc = "see or correct for color blindness", args = { "protan", "deutan", "tritan", "mono", "fix", "off" } })
