-- /version — surface the running smelt build identity as a notification.
--
-- The notification body matches `smelt --version` (the value embedded by
-- crates/tui/build.rs from `git describe --tags --long --dirty`) and
-- adds the build target + commit date so the user gets the same
-- information they'd see in a bug report.

local notify = smelt.notify.scoped("version")

smelt.cmd.register("version", function()
  local b = smelt.build or {}
  local label = b.version_string or b.version or "?"
  local extras = {}
  if b.target and b.target ~= "" and b.target ~= "unknown" then
    table.insert(extras, b.target)
  end
  if b.date then
    table.insert(extras, b.date)
  end
  if #extras > 0 then
    label = label .. " (" .. table.concat(extras, ", ") .. ")"
  end
  notify("smelt " .. label)
end, { desc = "show the running smelt build identity" })
