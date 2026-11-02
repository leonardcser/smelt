-- /mono <green|amber>: a monochrome screen. Every color in the theme becomes a shade of phosphor,
-- by how bright it is; green to start with.
local TINTS = {
  green = { dark = { 4, 16, 8 }, lit = { 80, 255, 120 } },
  amber = { dark = { 20, 10, 2 }, lit = { 255, 176, 40 } },
}
local function apply(name)
  local tint = TINTS[name]
  local function shade(c, low)
    if type(c) ~= "table" then return c end
    local l = low + (1 - low) * (0.3 * c[1] + 0.59 * c[2] + 0.11 * c[3]) / 255
    return {
      tint.dark[1] + (tint.lit[1] - tint.dark[1]) * l,
      tint.dark[2] + (tint.lit[2] - tint.dark[2]) * l,
      tint.dark[3] + (tint.lit[3] - tint.dark[3]) * l,
    }
  end
  local spec = smelt.theme.spec("Monokai Extended")
  for _, s in pairs(spec.groups) do
    if type(s) == "table" then
      s.fg, s.bg = s.fg and shade(s.fg, 0.35), s.bg and shade(s.bg, 0)
    end
  end
  spec.groups.Normal = { fg = tint.lit, bg = tint.dark }
  smelt.theme.apply(spec)
end
apply("green")

smelt.cmd.register("mono", function(arg)
  arg = (arg or ""):match("%S+") or ""
  if not TINTS[arg] then return smelt.notify.error("usage: /mono green|amber") end
  apply(arg)
end, { desc = "turn the screen green or amber", args = { "green", "amber" } })
