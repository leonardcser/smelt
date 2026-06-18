-- Built-in /goal command.

local goal = require("smelt.goal")

smelt.cmd.register("goal", function(arg)
  goal.command(arg)
end, {
  desc = "set, show, pause, resume, block, or clear the persistent session goal",
  args = { "[objective|set <objective>|status|pause|resume|block [reason]|done|clear|auto on|auto off]" },
  while_busy = false,
  queue_when_busy = true,
})
