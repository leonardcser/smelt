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
  smelt.notify.info("thinking blocks: " .. (changed and label or "unchanged"))
end, { desc = "set thinking block view state", args = { "open", "close", "peek", "toggle" } })

-- `/fast` - toggle accelerated inference for the current session.
smelt.cmd.register("fast", function(arg)
  local status = smelt.session.status()
  if not status.fast.supported then
    smelt.notify.error("fast mode is not supported by the current model")
    return
  end

  local enabled
  if not arg or arg == "" or arg:lower() == "toggle" then
    enabled = not status.fast.active
  elseif arg:lower() == "on" then
    enabled = true
  elseif arg:lower() == "off" then
    enabled = false
  else
    smelt.notify.error("usage: /fast [on|off|toggle]")
    return
  end

  smelt.session.set_fast_mode(enabled)
  smelt.notify.info("fast mode: " .. (enabled and "on" or "off"))
end, { desc = "toggle accelerated inference", args = { "on", "off", "toggle" } })

-- `/reasoning` - set explicitly or show current effort.
smelt.cmd.register("reasoning", function(arg)
  if arg then smelt.reasoning.set(arg) end
  smelt.notify.info("reasoning effort: " .. smelt.reasoning.current())
end, { desc = "set or show reasoning effort", args = smelt.reasoning.known_list() })
