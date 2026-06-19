local transcript_defaults = require("smelt.transcript.defaults")

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

transcript_defaults.__tool_body_renderers.switch_cwd = function()
  return nil
end

smelt.tools.register({
  name = "switch_cwd",
  description = "Switch Smelt's actual process working directory. Use this when you need Smelt itself, future relative tool calls, session metadata, prompt context, and workspace permissions to move to a different checkout or directory. This is different from running `cd` in bash, which only affects that one shell command.",
  effect = "write",
  headless = false,
  parameters = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Directory to switch Smelt into.",
      },
    },
    required = { "path" },
  },
  summary = function(args, ctx)
    return smelt.tools.path_summary(trim(args.path or ""), ctx)
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
