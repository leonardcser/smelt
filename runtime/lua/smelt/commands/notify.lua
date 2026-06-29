-- `/notify` controls transient turn-end terminal notifications.

local notifications = require("smelt.notifications")

local function show_status()
  smelt.notify.info("turn notifications: " .. notifications.status().mode)
end

smelt.cmd.register("notify", function(arg)
  local action = "once"
  if arg and arg ~= "" then action = arg:lower() end

  if action == "once" or action == "turn" or action == "next" then
    notifications.enable_once()
    smelt.notify.info("turn notifications: next turn")
  elseif action == "session" or action == "on" then
    notifications.enable_session()
    smelt.notify.info("turn notifications: session")
  elseif action == "off" then
    notifications.disable_session()
    smelt.notify.info("turn notifications: off for session")
  elseif action == "clear" then
    notifications.clear()
    show_status()
  elseif action == "status" then
    show_status()
  else
    smelt.notify.error("usage: /notify [once|on|off|clear|status]")
  end
end, {
  desc = "override turn-end notifications for this session",
  args = { "once", "on", "off", "clear", "status" },
})
