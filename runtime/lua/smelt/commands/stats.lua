-- Built-in /stats command.

smelt.cmd.register("stats", function()
  smelt.cmd.text_dialog("stats", smelt.metrics.stats_text())
end, { desc = "show token usage statistics" })
