-- A shell's prompt: one row with a `❯` that turns yellow while smelt works and green when it is
-- done, no rules around it. What the rules showed moves into the statusline.
local prompt = smelt.prompt
prompt.top:configure({ visible = false })
prompt.bottom:configure({ visible = false })

local COLORS = { working = { 240, 200, 80 }, busy = { 240, 200, 80 }, done = { 120, 220, 140 } }
local function caret()
  local color = COLORS[smelt.signal.get("work_state")] or { 130, 150, 255 }
  prompt.win:configure({ pad_left = 2, prefix = { first = "❯ ", style = { fg = color, bold = true } } })
end
caret()
smelt.signal.subscribe("work_state", caret)

local indicator = prompt.top:builtin("indicator")
smelt.statusline:remove("cursor")
smelt.statusline:add("indicator", {
  render = function(ctx)
    local items = indicator.render(ctx) or {}
    for _, item in ipairs(items) do item.align_right = true end
    return items
  end,
  signals = indicator.signals,
})
smelt.statusline:add("model", prompt.top:builtin("model"))
