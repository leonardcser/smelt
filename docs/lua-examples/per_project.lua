-- Example .smelt/init.lua for a repository.
-- Project-local config loads after /trust records its current content hash.

smelt.settings.show_tips = false

-- Let the test suite write build output in the sandbox without asking.
local check = smelt.permissions.check
smelt.permissions.check = function(call)
  local command = call.tool == "bash" and call.args.command or ""
  if call.access == "write" and command:match("^cargo test") then
    return "allow"
  end
  return check(call)
end

-- Let the agent work in a sibling checkout, and keep it out of the deploy keys.
smelt.permissions.paths({
  ["../shared-assets"] = "allow",
  ["./deploy/keys"] = "deny",
})

smelt.cmd.register("project-test", function()
  smelt.spawn(function()
    local result = smelt.process.run("cargo", { "test" })
    if result and result.exit_code == 0 then
      smelt.notify.info("cargo test passed")
    else
      smelt.notify.warn("cargo test failed")
    end
  end)
end, { desc = "run the project's test suite" })
