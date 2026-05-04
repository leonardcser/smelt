-- Built-in glob tool — fast file-pattern matching, gitignore-aware.
-- Composes `tui::fs::glob` (globset + ignore::WalkBuilder) and returns
-- matching paths sorted newest-first by mtime.

local function confirm_message(args)
  local pattern = args.pattern or ""
  local path = args.path or ""
  if path == "" then
    return pattern
  end
  return pattern .. " in " .. path
end

smelt.tools.register({
  name = "glob",
  description = "Fast file pattern matching tool that works with any codebase size. Returns matching file paths sorted by modification time.",
  override = true,
  parameters = {
    type = "object",
    properties = {
      pattern = {
        type = "string",
        description = "The glob pattern to match files against (supports **), e.g. **/*.rs",
      },
      path = {
        type = "string",
        description = "The directory to search in. If not specified, the current working directory will be used.",
      },
    },
    required = { "pattern" },
  },
  needs_confirm = function(args)
    return confirm_message(args)
  end,
  render = function(args, output, width, ctx)
    local content = output.content or ""
    local n = 0
    if content ~= "" then
      local _, newlines = content:gsub("\n", "\n")
      n = newlines
      if content:sub(-1) ~= "\n" then
        n = n + 1
      end
    end
    ctx:text(n .. " files")
  end,
  execute = function(args)
    local pattern = args.pattern or ""
    if pattern == "" then
      return { content = "missing required parameter: pattern", is_error = true }
    end
    local path = args.path or ""
    local results, err = smelt.fs.glob(pattern, path, { max = 200 })
    if err then
      return { content = err, is_error = true }
    end
    if not results or #results == 0 then
      return "no matches found"
    end
    return table.concat(results, "\n")
  end,
})
