local transcript_defaults = require("smelt.transcript.defaults")
local worktree = require("smelt.worktree")

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

transcript_defaults.__tool_body_renderers.enter_worktree = function(block)
  local info = block.output and block.output.metadata
  if not info then return nil end
  return worktree.detail(info)
end

smelt.tools.register({
  name = "enter_worktree",
  description = "Create or open a managed git worktree and switch Smelt's actual process working directory into it. Use this when the user asks to implement in a new worktree.",
  effect = "write",
  headless = false,
  parameters = {
    type = "object",
    properties = {
      name = {
        type = "string",
        description = "Semantic worktree name. Smelt lowercases it, replaces spaces with dashes, removes unsafe folder characters, and deduplicates conflicts.",
      },
      base = {
        type = "string",
        description = "Optional git base ref for new worktrees (default: main if present, else master, else HEAD).",
      },
    },
    required = { "name" },
  },
  summary = function(args)
    return worktree.display_name(args.name or "")
  end,
  execute = function(args)
    local name = trim(args.name or "")
    if name == "" then
      return { content = "name is required", is_error = true }
    end
    local info, err = worktree.enter({
      name = name,
      base = args.base or "",
    })
    if not info then
      return { content = err, is_error = true }
    end
    return {
      content = worktree.instructions(info),
      metadata = info,
    }
  end,
})
