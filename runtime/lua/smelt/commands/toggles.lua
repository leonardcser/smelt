-- Built-in `/vim` and `/thinking` toggles.
--
-- Direct shortcuts for the two most-toggled boolean settings. The
-- `/settings` picker covers the same ground via a menu; these are the
-- single-keystroke aliases users reach for during a session.

smelt.cmd.register("vim", function()
  smelt.settings.vim = not smelt.settings.vim
end, { desc = "toggle vim mode" })

smelt.cmd.register("thinking", function()
  smelt.settings.show_thinking = not smelt.settings.show_thinking
end, { desc = "toggle thinking blocks" })
