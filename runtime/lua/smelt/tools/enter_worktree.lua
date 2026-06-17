local transcript_defaults = require("smelt.transcript.defaults")

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local function worktree_display_name(name)
  local out = {}
  local last_dash = false
  for ch in trim(name):lower():gmatch(".") do
    local dash = ch:match("%s") or ch == "-" or ch == "_" or ch == "." or ch == "/"
    if ch:match("%w") then
      out[#out + 1] = ch
      last_dash = false
    elseif dash and not last_dash and #out > 0 then
      out[#out + 1] = "-"
      last_dash = true
    end
    if #out >= 64 then break end
  end
  local s = table.concat(out):gsub("%-+$", "")
  return s ~= "" and s or trim(name)
end

local function worktree_instructions(info)
  return table.concat({
    "entered managed worktree " .. info.name,
    "path: " .. info.path,
    "branch: " .. info.branch,
    "base: " .. info.base,
  }, "\n")
end

local function worktree_detail(info)
  return smelt.layout.runs({
    { { text = "branch", dim = true }, { text = "  " }, { text = info.branch or "" } },
    { { text = "base", dim = true }, { text = "    " }, { text = info.base or "" } },
    { { text = "path", dim = true }, { text = "    " }, { text = info.path or "" } },
  })
end

transcript_defaults.__tool_body_renderers.enter_worktree = function(block)
  local info = block.output and block.output.metadata
  if not info then return nil end
  return worktree_detail(info)
end

smelt.tools.register({
  name = "enter_worktree",
  description = "Create or open a managed git worktree and switch Smelt's actual process working directory into it. Use this when the user asks to implement in a new worktree.",
  effect = "write",
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
    return worktree_display_name(args.name or "")
  end,
  execute = function(args)
    local name = trim(args.name or "")
    if name == "" then
      return { content = "name is required", is_error = true }
    end
    local ok, info = pcall(smelt.session.enter_worktree, {
      name = name,
      base = trim(args.base or ""),
    })
    if not ok then
      return { content = tostring(info), is_error = true }
    end
    return {
      content = worktree_instructions(info),
      metadata = info,
    }
  end,
})
