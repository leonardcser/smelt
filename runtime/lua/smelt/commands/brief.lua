-- Built-in /brief command.

local skills = require("smelt.skills")

local function parse_arg(arg)
  arg = arg or ""
  local scope, focus = arg:match("^(%S+)%s*(.*)$")
  if scope == "user" or scope == "internal" or scope == "all" then
    return scope, focus or ""
  end
  return "user", arg
end

smelt.cmd.register("brief", function(arg)
  local body, err = skills.body("brief")
  if not body then
    smelt.notify.error(err or "brief skill not found")
    return
  end

  local scope, focus = parse_arg(arg)
  body = body .. "\n\n## Requested scope\n\n" .. scope
  if focus ~= "" then
    body = body .. "\n\n## Focus\n\n" .. focus
  end

  local display = "brief"
  if arg and arg ~= "" then display = display .. " " .. arg end
  smelt.engine.submit_command("brief", body, nil, display)
end, {
  desc = "summarize planned or completed changes compactly",
  args = { "[user|internal|all]", "<focus>" },
  busy = "queue_request",
})
