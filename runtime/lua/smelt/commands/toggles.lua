-- `/vim` and `/thinking` toggles (quick aliases for the two most-used settings).

smelt.cmd.register("vim", function()
  smelt.settings.vim = not smelt.settings.vim
  smelt.notify("vim mode: " .. (smelt.settings.vim and "on" or "off"))
end, { desc = "toggle vim mode" })

smelt.cmd.register("thinking", function()
  smelt.settings.show_thinking = not smelt.settings.show_thinking
  smelt.notify("thinking blocks: " .. (smelt.settings.show_thinking and "on" or "off"))
end, { desc = "toggle thinking blocks" })

-- `/reasoning` - set explicitly or show current effort.
smelt.cmd.register("reasoning", function(arg)
  local valid = { off = true, low = true, medium = true, high = true, max = true }
  if arg then
    arg = arg:lower()
    if valid[arg] then
      smelt.reasoning(arg)
    else
      smelt.notify.error("invalid reasoning effort: " .. arg)
      return
    end
  end
  smelt.notify("reasoning effort: " .. smelt.reasoning())
end, { desc = "set or show reasoning effort", args = { "off", "low", "medium", "high", "max" } })
