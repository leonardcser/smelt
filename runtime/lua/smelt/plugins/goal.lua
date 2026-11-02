-- Goal lifecycle plugin. Registers the goal tools, headerline source, context
-- note sync, and goal-aware auto-continue provider.

if not smelt.frontend.is_interactive() then return end

require("smelt.goal").setup()
require("smelt.goal.command")
smelt.tips.register({
  id = "commands.goal",
  key = "/goal",
  text = "keep a persistent objective visible across turns",
})
