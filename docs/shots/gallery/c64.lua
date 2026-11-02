-- A Commodore 64: light blue on blue inside a light blue border, BASIC up top, READY. below.
local BLUE, LIGHT = { 64, 49, 141 }, { 134, 122, 222 }
local spec = smelt.theme.spec("base16")
for _, s in pairs(spec.groups) do
  if type(s) == "table" then
    if s.fg then s.fg = LIGHT end
    if s.bg then s.bg = { 82, 68, 170 } end
  end
end
spec.groups.Normal = { fg = LIGHT, bg = BLUE }
spec.groups.SmeltStatusBg = { bg = BLUE }
smelt.theme.apply(spec)

local ui, compose = smelt.ui.layout, require("smelt.layout").compose
local function edge() return smelt.paint.register(function(s) s:fill(0, 0, s:width(), s:height(), " ", { bg = LIGHT }) end) end
local top, left, right, bottom = edge(), edge(), edge(), edge()
ui.set(function(state)
  local inner = setmetatable({ term_w = state.term_w - 6, term_h = state.term_h - 2 }, { __index = state })
  return ui.vbox({
    { ui.leaf(top), height = 1 },
    { ui.hbox({ { ui.leaf(left), width = 3 }, { compose(inner), width = "fill" }, { ui.leaf(right), width = 3 } }), height = "fill" },
    { ui.leaf(bottom), height = 1 },
  })
end)

smelt.headerline:add("c64", function(ctx)
  local title = "**** SMELT 64 BASIC V2 ****"
  return { text = (" "):rep(math.max(0, (ctx.width - #title) // 2)) .. title }
end)
local on = true
smelt.statusline:clear()
smelt.statusline:add("ready", function()
  return {
    { text = "READY. " .. (on and "█" or " "), style = { fg = LIGHT } },
    { text = (smelt.signal.get("agent_mode") or "normal") .. " ", style = { fg = LIGHT }, align_right = true },
  }
end)
smelt.timer.every(500, function()
  on = not on
  smelt.statusline:invalidate()
end)
