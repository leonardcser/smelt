-- `/thinking` folds or unfolds thinking blocks for the current session.

smelt.cmd.register("thinking", function(arg)
  local action = "toggle"
  if arg and arg ~= "" then
    action = arg:lower()
  end
  if action == "on" or action == "open" or action == "show" then
    action = "open"
  elseif action == "off" or action == "close" or action == "hide" then
    action = "close"
  elseif action ~= "toggle" and action ~= "peek" then
    smelt.notify.error("usage: /thinking [open|close|peek|toggle]")
    return
  end
  local changed = smelt.transcript.fold_kind("thinking", action)
  local label = action == "toggle" and "toggled" or action
  smelt.notify("thinking blocks: " .. (changed and label or "unchanged"))
end, { desc = "set thinking block view state", args = { "open", "close", "peek", "toggle" } })

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
