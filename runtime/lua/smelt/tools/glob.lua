-- Built-in glob tool. Gitignore-aware pattern matching; results sorted newest-first.

local function describe(args)
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
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
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
  summary = function(args)
    return describe(args)
  end,
  render = function(_, output)
    if output.is_error then
      return smelt.layout.text(output.content, { hl_group = "ErrorMsg" })
    end
    return smelt.layout.text(smelt.text.line_count(output.content or "") .. " files")
  end,
  paths_for_workspace = function(args)
    local p = args.path or ""
    return p ~= "" and { p } or {}
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
