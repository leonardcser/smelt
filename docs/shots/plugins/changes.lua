-- ~/.config/smelt/plugins/changes.lua
local layout = smelt.ui.layout
local default = require("smelt.layout").compose
local files = smelt.win.new(smelt.buf.new(), { surface = "list_inert", wrap = false, pad_left = 1 })
-- A solid panel in the scroll pill's color.
files:row_highlights({ { start = 0, ["end"] = 1000, hl_group = "SmeltScrollPillBg" } })

-- Re-read the working tree after every tool the agent runs.
local function refresh()
  smelt.spawn(function()
    local snap = smelt.git.diff()
    files:document(snap and snap.diff:tree():document())
  end)
end
smelt.events.on("tool_end", refresh)
refresh()

layout.set(function(state)
  return layout.hbox({
    { default(state), width = "fill" },
    { layout.leaf(files), width = 34 },
  })
end)
