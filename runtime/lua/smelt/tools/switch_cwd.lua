local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

smelt.transcript.register_tool("switch_cwd", {
  cache_key = "smelt.tool-presentation.switch_cwd:v1",
  body = function() return nil end,
})

smelt.tools.register({
  name = "switch_cwd",
  description = "Switch smelt's actual process working directory. Use this when you need smelt itself, future relative tool calls, session metadata, prompt context, and workspace permissions to move to a different checkout or directory. This is different from running `cd` in bash, which only affects that one shell command.",
  effect = "write",
  execution_mode = "sequential",
  headless = false,
  parameters = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Directory to switch smelt into. May be absolute, relative to the current cwd, or start with `~`.",
      },
    },
    required = { "path" },
  },
  summary = function(args, ctx)
    return smelt.tools.path_summary(trim(args.path or ""), ctx)
  end,
  paths_for_workspace = function(args)
    local path = trim(args.path or "")
    return path ~= "" and { { path = smelt.path.resolve(path), kind = "directory" } } or {}
  end,
  execute = function(args)
    local path = trim(args.path or "")
    if path == "" then
      return { content = "path is required", is_error = true }
    end
    local ok, out = pcall(smelt.session.switch_cwd, path)
    if not ok then
      return { content = tostring(out), is_error = true }
    end
    return {
      content = "cwd: " .. out.cwd,
      metadata = out,
    }
  end,
})
