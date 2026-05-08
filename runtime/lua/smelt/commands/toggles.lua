-- `/vim` and `/thinking` toggles (quick aliases for the two most-used settings).

smelt.cmd.register("vim", function()
  smelt.settings.vim = not smelt.settings.vim
end, { desc = "toggle vim mode" })

smelt.cmd.register("thinking", function()
  smelt.settings.show_thinking = not smelt.settings.show_thinking
end, { desc = "toggle thinking blocks" })
