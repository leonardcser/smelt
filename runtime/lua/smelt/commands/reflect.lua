-- Built-in /reflect command.

local skills = require("smelt.skills")

smelt.cmd.register("reflect", function(arg)
  local body, err = skills.body("reflect")
  if not body then
    smelt.notify.error(err or "reflect skill not found")
    return
  end

  local display = "reflect"
  if arg and arg ~= "" then
    body = body .. "\n\n## Additional Focus\n\n" .. arg
    display = display .. " " .. arg
  end
  smelt.engine.submit_command("reflect", body, nil, display)
end, {
  desc = "step back and rethink recent changes before moving on",
  args = { "<focus>" },
  while_busy = false,
  queue_when_busy = true,
})
