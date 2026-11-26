-- /cwd [path] - show or switch smelt's working directory, like the
-- `switch_cwd` tool. The user names the path, so no permission dialog is shown.

local argv = require("smelt.argv")

local function command(arg)
  local tokens, err = argv.split(arg)
  if not tokens then
    smelt.notify.error(err)
    return
  end
  if #tokens == 0 then
    smelt.notify.info("cwd: " .. smelt.session.cwd())
    return
  end
  if #tokens > 1 then
    smelt.notify.error("usage: /cwd [path]")
    return
  end
  local ok, out = pcall(smelt.session.switch_cwd, tokens[1])
  if not ok then
    smelt.notify.error(tostring(out))
    return
  end
  smelt.notify.info("switching to " .. out.cwd)
end

smelt.cmd.register("cwd", command, {
  desc = "show or switch the working directory",
  args = { "[path]" },
  busy = "reject",
})
