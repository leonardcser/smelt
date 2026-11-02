-- Vaporwave: dark to purple, mid to pink,
-- bright to cyan.
local spec = smelt.theme.spec("Dracula")
local RAMP = { { 34, 12, 62 }, { 255, 64, 170 },
  { 120, 245, 255 } }
local function wave(c)
  if type(c) ~= "table" then return c end
  local l = (c[1] + c[2] + c[3]) / 383
  local i = math.min(math.floor(l), 1)
  local a, b, k = RAMP[i + 1], RAMP[i + 2], l - i
  return { a[1] + (b[1] - a[1]) * k,
    a[2] + (b[2] - a[2]) * k, a[3] + (b[3] - a[3]) * k }
end
for _, s in pairs(spec.groups) do
  if type(s) == "table" then
    s.fg = s.fg and wave(s.fg)
    s.bg = s.bg and wave(s.bg)
  end
end
spec.groups.Normal = {
  fg = { 255, 200, 240 },
  bg = RAMP[1],
}
smelt.theme.apply(spec)
