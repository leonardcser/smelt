-- Built-in /goal command.

local goal = require("smelt.goal")

smelt.cmd.register("goal", function(arg)
  goal.command(arg)
end, {
  desc = "manage the persistent session goal: set, status, activity, progress, summary, pause, resume, block, done, clear, or auto",
  args = { "[objective|set <objective>|status|activity <text>|progress <label>|summary <label>|pause|resume|block [reason]|done|clear|auto on|auto off]" },
  while_busy = false,
  queue_when_busy = true,
})
