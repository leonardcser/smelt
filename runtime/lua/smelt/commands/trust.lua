-- /trust — record the SHA-256 hash of the current `<cwd>/.smelt/`
-- content so smelt will load `init.lua` and `plugins/*.lua` from it on
-- subsequent startups. Edits invalidate the hash and require running
-- /trust again.

smelt.cmd.register("trust", function()
  local status = smelt.trust.status()
  if status == "no_content" then
    smelt.notify("no .smelt/ content under this project; nothing to trust")
    return
  end
  local ok, hash = pcall(smelt.trust.mark)
  if not ok then
    smelt.notify_error("trust: " .. tostring(hash))
    return
  end
  smelt.notify("project trusted: " .. hash:sub(1, 12) .. "; restart to load .smelt/")
end, {
  desc       = "trust the current project's .smelt/ content",
  startup_ok = true,
})
