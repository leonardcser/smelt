-- Built-in /goal command.

local goal = require("smelt.goal")

smelt.cmd.register("goal", function(arg)
  goal.command(arg)
end, {
  desc = "set, inspect, pause, resume, block, or clear the persistent session goal",
  args = { "[objective|status|pause|resume|block|done|clear|auto on|auto off]" },
  while_busy = false,
  queue_when_busy = true,
})
