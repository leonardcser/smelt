-- Example .smelt/init.lua for a repository.
-- Project-local config loads after /trust records its current content hash.

smelt.settings.show_tips = false

smelt.permissions.extend({
  default = {
    patterns = {
      bash = {
        allow = { "git status *", "git diff *", "cargo test *" },
      },
    },
  },
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
