-- Example .smelt/init.lua for a repository.
-- Project-local config is loaded automatically after you run /trust once.

smelt.settings.show_tips = false

smelt.permissions.set_rules({
  default = {
    bash = {
      allow = { "git status *", "git diff *", "cargo test *" },
    },
  },
})

smelt.cmd.register("project-test", function()
  smelt.spawn(function()
    local result = smelt.process.run("cargo", { "test" })
    if result and result.exit_code == 0 then
      smelt.notify("cargo test passed")
    else
      smelt.notify.warn("cargo test failed")
    end
  end)
end, { desc = "run the project's test suite" })
