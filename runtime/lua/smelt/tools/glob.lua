-- Built-in glob tool. Gitignore-aware pattern matching; results sorted newest-first.

local transcript_defaults = require("smelt.transcript.defaults")

local function describe(args, ctx)
  local pattern = args.pattern or ""
  local path = args.path or ""
  if path == "" then
    return pattern
  end
  local summary = smelt.tools.path_summary(path, ctx, { prefix = pattern .. " in " })
  if summary == "" then return pattern end
  return summary
end

local function glob_collapsed_detail(block)
  local output = block.output

  local metadata = output and output.metadata
  if type(metadata) == "table" and type(metadata.display_count) == "table" then
    return transcript_defaults.display_count_text(block, { unit = "file" })
  end

  return tostring((output and output.content_lines) or 0) .. " files"
end

smelt.transcript.register_tool("glob", {
  cache_key = "smelt.tool-presentation.glob:v1",
  body = function(block, ctx)
    if not block.output then return nil end
    return transcript_defaults.render_tool_output_tail(block.output, ctx)
  end,
  compact = glob_collapsed_detail,
})

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
  summary = function(args, ctx)
    return describe(args, ctx)
  end,
  paths_for_workspace = function(args)
    local p = args.path or ""
    return p ~= "" and { { path = p, kind = "directory" } } or {}
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
    local paths = smelt.tools._compact_cwd_paths(results and results.paths or {})
    if results and results.timed_out then
      return {
        content = string.format(
          "timed out after %.1fs while scanning %d files%s",
          timeout_ms / 1000,
          results.scanned or 0,
          #paths > 0 and ("\n" .. table.concat(paths, "\n")) or ""
        ),
        is_error = true,
        metadata = { display_count = { value = #paths, unit = "file" } },
      }
    end
    if results and results.scan_limit_hit then
      local suffix = #paths > 0 and ("\n" .. table.concat(paths, "\n")) or ""
      return {
        content = string.format("search stopped after scanning %d files%s", results.scanned or 0, suffix),
        metadata = { display_count = { value = #paths, unit = "file" } },
      }
    end
    if results and results.truncated then
      local suffix = #paths > 0 and ("\n" .. table.concat(paths, "\n")) or ""
      return {
        content = string.format("showing first %d matches%s", #paths, suffix),
        metadata = { display_count = { value = #paths, unit = "file" } },
      }
    end
    if #paths == 0 then
      return {
        content = "no matches found",
        metadata = { display_count = { value = 0, unit = "file" } },
      }
    end
    return {
      content = table.concat(paths, "\n"),
      metadata = { display_count = { value = #paths, unit = "file" } },
    }
  end,
}, { default_ms = 30000, max_ms = 120000, grace_ms = 5000 }))
