-- Built-in /handoff command.

local skills = require("smelt.skills")

smelt.cmd.register("handoff", function(arg)
  local body, err = skills.body("handoff")
  if not body then
    smelt.notify.error(err or "handoff skill not found")
    return
  end

  local display = "handoff"
  if arg and arg ~= "" then
    body = body .. "\n\n## Additional Focus\n\n" .. arg
    display = display .. " " .. arg
  end
  smelt.engine.submit_command("handoff", body, nil, display)
end, {
  desc = "write a continuation handoff for another agent",
  args = { "<focus>" },
  busy = "queue_request",
})
