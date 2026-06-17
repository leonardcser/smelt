-- A small hot-reload-friendly plugin.
-- Copy to ~/.config/smelt/lua/example_plugin.lua, then add
-- `require("example_plugin")` to ~/.config/smelt/init.lua.

local M = {}

local state = smelt.state("example_plugin")
state.pings = state.pings or 0

local function ping(arg)
  state.pings = (state.pings or 0) + 1
  local target = (arg and arg ~= "") and arg or "smelt"
  smelt.notify(string.format("ping %s (%d)", target, state.pings))
end

smelt.cmd.register("example-ping", ping, {
  desc = "increment the example plugin counter",
  args = { "name?" },
})

-- UiHost APIs are available in on_ready. The hook is registered in module body,
-- so it fires again after /reload and replaces the statusline source in place.
smelt.lifecycle.on_ready(function(ctx)
  local statusline = require("smelt.statusline")
  statusline.add("example_plugin", function()
    if (state.pings or 0) == 0 then return {} end
    return { {
      text = " pings " .. state.pings,
      style = { fg = "SmeltAccent" },
      priority = 4,
      separated = true,
    } }
  end)

  if ctx.kind == "reload" then
    smelt.notify("example plugin reloaded")
  end
end)

return M
