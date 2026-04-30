-- Built-in /compact command.

smelt.cmd.register("compact", function(arg)
  smelt.engine.compact(arg)
end, { desc = "compact conversation history" })
