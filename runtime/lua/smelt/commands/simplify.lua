-- Built-in /simplify command.

local skills = require("smelt.skills")

smelt.cmd.register("simplify", function(arg)
  local body, err = skills.body("simplify")
  if not body then
    smelt.notify.error(err or "simplify skill not found")
    return
  end

  local display = "simplify"
  if arg and arg ~= "" then
    body = body .. "\n\n## Additional Focus\n\n" .. arg
    display = display .. " " .. arg
  end
  smelt.engine.submit_command("simplify", body, nil, display)
end, {
  desc = "review changed code for reuse, quality, and efficiency",
  args = { "<focus>" },
  while_busy = false,
  queue_when_busy = true,
})
