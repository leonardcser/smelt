-- ~/.config/smelt/plugins/crates.lua
smelt.tools.register({
  name = "crate_info",
  description = "Latest version and downloads of a crate on crates.io.",
  parameters = {
    type = "object",
    properties = { name = { type = "string" } },
    required = { "name" },
  },
  execute = function(args)
    local url = "https://crates.io/api/v1/crates/" .. args.name
    local headers = { ["user-agent"] = "smelt" }
    local res, err = smelt.http.get(url, { headers = headers })
    if not res or res.status ~= 200 then
      return { content = err or "crate not found", is_error = true }
    end
    local c = smelt.json.decode(res.body).crate
    return ("%s %s, %d downloads: %s"):format(
      c.name, c.max_stable_version, c.downloads, c.description)
  end,
})
