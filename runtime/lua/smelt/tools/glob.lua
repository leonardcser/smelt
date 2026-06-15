-- Built-in glob tool. Gitignore-aware pattern matching; results sorted newest-first.

local function describe(args)
  local pattern = args.pattern or ""
  local path = args.path or ""
  if path == "" then
    return pattern
  end
  return pattern .. " in " .. path
end

smelt.tools.register(smelt.tools._with_watchdog({
  name = "glob",
  description = "Fast file pattern matching tool that works with any codebase size. Returns matching file paths sorted by modification time.",
  override = true,
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  effect = "read",
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
      timeout_ms = {
        type = "integer",
        description = "Timeout in milliseconds (default: 30000)",
      },
    },
    required = { "pattern" },
  },
  summary = function(args)
    return describe(args)
  end,
  render = function(_, output, ctx)
    if output.is_error then
      return smelt.layout.tool_output(output, ctx)
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
    local max = tonumber(args.max) or 200
    local timeout_ms = tonumber(args.timeout_ms) or 30000
    local results, err = smelt.fs.glob_async(pattern, path, {
      max = max,
      max_scanned = tonumber(args.max_scanned) or 100000,
      timeout_ms = timeout_ms,
    })
    if err then
      return { content = err, is_error = true }
    end
    local paths = results and results.paths or {}
    if results and results.timed_out then
      return {
        content = string.format(
          "timed out after %.1fs while scanning %d files%s",
          timeout_ms / 1000,
          results.scanned or 0,
          #paths > 0 and ("\n" .. table.concat(paths, "\n")) or ""
        ),
        is_error = true,
      }
    end
    if results and results.scan_limit_hit then
      local suffix = #paths > 0 and ("\n" .. table.concat(paths, "\n")) or ""
      return string.format("search stopped after scanning %d files%s", results.scanned or 0, suffix)
    end
    if results and results.truncated then
      local suffix = #paths > 0 and ("\n" .. table.concat(paths, "\n")) or ""
      return string.format("showing first %d matches%s", #paths, suffix)
    end
    if #paths == 0 then
      return "no matches found"
    end
    return table.concat(paths, "\n")
  end,
}, { default_ms = 30000, max_ms = 120000, grace_ms = 5000 })
