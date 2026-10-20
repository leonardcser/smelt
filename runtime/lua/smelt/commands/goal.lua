-- Built-in /goal command.

local goal = require("smelt.goal")

smelt.cmd.register("goal", function(arg)
  goal.command(arg)
end, {
  desc = "manage the persistent session goal: set, status, progress, summary, pause, resume, block, done, clear (aliases: cancel, stop), or auto",
  args = { "[objective|set <objective>|status|progress <label>|summary <label>|pause|resume|block [reason]|done|clear|cancel|stop|auto on|auto off]" },
  busy = "queue_command",
  busy_fn = goal.command_busy_behavior,
})
