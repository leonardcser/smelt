-- The headerline: a strip bar above the transcript. It has no built-in
-- sources; features such as goals add theirs with
-- `smelt.headerline:add(name, handler)`. The default layout reserves rows only
-- while some source shows an item.

local bar = require("smelt._bar")

--- The strip above the transcript, a `smelt.bar.Bar`. It takes rows only
--- while some source shows an item.
---@tier ui_host
---@type smelt.bar.Bar
smelt.headerline = bar.new({
  name = "smelt.headerline",
  layout = "strip",
  min_rows = 0,
  -- A header is content (a goal, a title), so drag-copy takes the whole row.
  fill_selectable = true,
})

smelt.signal.subscribe("session_epoch", function() smelt.headerline:invalidate() end)

return smelt.headerline
