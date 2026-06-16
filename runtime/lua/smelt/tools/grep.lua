-- Built-in grep tool. Uses ripgrep, falls back to grep when rg is absent.

local transcript_defaults = require("smelt.transcript.defaults")

local function pick_bool(v, default)
  if type(v) == "boolean" then return v end
  return default
end

local function pick_int(v, default)
  if type(v) == "number" then return math.floor(v) end
  return default or 0
end

local function normalize_glob(v)
  if type(v) ~= "string" then return nil end
  if v == "" or v == "*" or v == "**" or v == "**/*" or v == "./*" or v == "./**/*" then
    return nil
  end
  return v
end

local function as_lines(content)
  if not content or content == "" then return {} end
  local lines = {}
  local start = 1
  local len = #content
  while start <= len do
    local nl = content:find("\n", start, true)
    if nl then
      table.insert(lines, content:sub(start, nl - 1))
      start = nl + 1
    else
      table.insert(lines, content:sub(start))
      break
    end
  end
  return lines
end

local function slice(content, offset, head_limit)
  if (offset == 0) and (head_limit == 0) then return content end
  local lines = as_lines(content)
  local start = math.min(offset, #lines)
  local stop = #lines
  if head_limit > 0 then stop = math.min(start + head_limit, #lines) end
  local out = {}
  for i = start + 1, stop do table.insert(out, lines[i]) end
  return table.concat(out, "\n")
end

local function combine_streams(stdout, stderr)
  local combined = stdout or ""
  if stderr and stderr ~= "" then
    if combined ~= "" then combined = combined .. "\n" end
    combined = combined .. stderr
  end
  return combined
end

local function requested_mode(args)
  local mode = args.output_mode
  if mode == nil or mode == "" then return "files_with_matches" end
  return mode
end

local function count_unit_for_mode(mode)
  if mode == "files_with_matches" then return "file" end
  if mode == "content" then return "line" end
  return "match"
end

local function sum_count_output(lines)
  local total = 0
  for _, line in ipairs(lines) do
    local n = line:match(":(%d+)$") or line:match("^(%d+)$")
    if n then total = total + tonumber(n) end
  end
  return total
end

local function grep_result_metadata(content, mode)
  local lines = as_lines(content)
  local count = #lines
  if mode == "count" then count = sum_count_output(lines) end
  return { display_count = { value = count, unit = count_unit_for_mode(mode) } }
end

local function grep_result(content, source, mode)
  return { content = content, metadata = grep_result_metadata(source, mode) }
end

local function no_matches_result(mode)
  return {
    content = "no matches found",
    metadata = { display_count = { value = 0, unit = count_unit_for_mode(mode) } },
  }
end

local function run_rg(args)
  local pattern = args.pattern or ""
  local path = args.path or ""
  local mode = args.output_mode
  if mode == nil or mode == "" then mode = "files_with_matches" end

  local context = pick_int(args.context, 0)
  if context == 0 then context = pick_int(args["-C"], 0) end

  local glob_filter = normalize_glob(args.glob)
  local file_type = args.type
  if file_type == "" then file_type = nil end

  local timeout_ms = pick_int(args.timeout_ms, 30000)
  if timeout_ms <= 0 then timeout_ms = 30000 end

  local opts = {
    mode = mode,
    case_insensitive = pick_bool(args["-i"], false),
    multiline = pick_bool(args.multiline, false),
    line_numbers = pick_bool(args["-n"], true),
    after_context = pick_int(args["-A"], 0),
    before_context = pick_int(args["-B"], 0),
    context = context,
    glob = glob_filter,
    type = file_type,
    include_ignored = pick_bool(args.include_ignored, false),
    timeout_secs = math.max(1, math.floor(timeout_ms / 1000)),
  }
  return smelt.grep.run(pattern, path, opts)
end

local function run_grep_fallback(args)
  local pattern = args.pattern or ""
  local search_path = args.path or ""
  if search_path == "" then search_path = "." end
  local case_insensitive = pick_bool(args["-i"], false)
  local glob_filter = normalize_glob(args.glob)
  local timeout_ms = pick_int(args.timeout_ms, 30000)
  if timeout_ms <= 0 then timeout_ms = 30000 end

  local cmd_args = { "-rn", "--max-count=200" }
  if not pick_bool(args.include_ignored, false) then
    for _, dir in ipairs({ ".git", ".jj", ".hg", ".svn", ".sl", ".worktrees", "target", "node_modules" }) do
      table.insert(cmd_args, "--exclude-dir=" .. dir)
    end
  end
  if case_insensitive then table.insert(cmd_args, "-i") end
  if glob_filter and glob_filter ~= "" then
    table.insert(cmd_args, "--include=" .. glob_filter)
  end
  table.insert(cmd_args, "--")
  table.insert(cmd_args, pattern)
  table.insert(cmd_args, search_path)

  local timeout_secs = math.max(1, math.floor(timeout_ms / 1000))
  return smelt.process.run("grep", cmd_args, { timeout_secs = timeout_secs })
end

local function grep_collapsed_detail(block)
  local output = block.output
  if output and output.is_error then return "error" end

  local mode = requested_mode((block and block.args) or {})
  local metadata = output and output.metadata
  if type(metadata) == "table" and type(metadata.display_count) == "table" then
    return transcript_defaults.display_count_text(block, { unit = count_unit_for_mode(mode) })
  end

  return smelt.text.line_count((output and output.content) or "") .. " matches"
end

transcript_defaults.__tool_body_renderers.grep = function(block, ctx)
  if not block.output then return nil end
  return transcript_defaults.render_tool_output_tail(block.output, ctx)
end

transcript_defaults.__tool_collapsed_details.grep = grep_collapsed_detail

smelt.tools.register(smelt.tools._with_watchdog({
  name = "grep",
  description = "A powerful search tool built on ripgrep. Supports full regex syntax, file type filtering, glob filtering, and multiple output modes.",
  override = true,
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  effect = "read",
  parameters = {
    type = "object",
    properties = {
      pattern = { type = "string", description = "The regular expression pattern to search for in file contents" },
      path = { type = "string", description = "File or directory to search in. Defaults to current working directory." },
      glob = { type = "string", description = 'Glob pattern to filter files (e.g. "*.js", "*.{ts,tsx}")' },
      type = { type = "string", description = "File type to search (rg --type). Common types: js, py, rust, go, java." },
      output_mode = {
        type = "string",
        ["enum"] = { "content", "files_with_matches", "count" },
        description = 'Output mode: "content" shows matching lines, "files_with_matches" shows file paths (default), "count" shows match counts.',
      },
      ["-i"] = { type = "boolean", description = "Case insensitive search (rg -i)" },
      ["-n"] = { type = "boolean", description = 'Show line numbers in output (rg -n). Requires output_mode: "content", ignored otherwise. Defaults to true.' },
      ["-A"] = { type = "integer", description = 'Number of lines to show after each match (rg -A). Requires output_mode: "content", ignored otherwise.' },
      ["-B"] = { type = "integer", description = 'Number of lines to show before each match (rg -B). Requires output_mode: "content", ignored otherwise.' },
      ["-C"] = { type = "integer", description = "Alias for context." },
      context = { type = "integer", description = 'Number of lines to show before and after each match. Only applies to output_mode "content".' },
      multiline = { type = "boolean", description = "Enable multiline mode where . matches newlines and patterns can span lines." },
      head_limit = { type = "integer", description = "Limit output to first N lines/entries. Defaults to 250; 0 means unlimited." },
      offset = { type = "integer", description = "Skip first N lines/entries before applying head_limit." },
      include_ignored = { type = "boolean", description = "Search ignored files and directories. Defaults to false." },
      timeout_ms = { type = "integer", description = "Timeout in milliseconds (default: 30000)" },
    },
    required = { "pattern" },
  },
  summary = function(args)
    local pattern = args.pattern or ""
    local path = args.path or ""
    if path == "" then return pattern end
    return pattern .. " in " .. smelt.path.display(path)
  end,
  paths_for_workspace = function(args)
    local p = args.path or ""
    return p ~= "" and { { path = p, kind = "directory" } } or {}
  end,
  execute = function(args)
    local offset = pick_int(args.offset, 0)
    local head_limit = pick_int(args.head_limit, 250)
    local mode = requested_mode(args)
    local result_mode = mode

    local out, err = run_rg(args)
    if not out then
      out, err = run_grep_fallback(args)
      result_mode = "content"
      if not out then
        return { content = err or "grep failed", is_error = true }
      end
    end

    local combined = combine_streams(out.stdout, out.stderr)
    local count_source = out.stdout or combined
    if out.timed_out then
      local secs = math.floor(((args.timeout_ms or 30000) / 1000) + 0.5)
      return {
        content = string.format("timed out after %ds", secs),
        is_error = true,
        metadata = grep_result_metadata(count_source, result_mode),
      }
    end

    local exit_code = out.exit_code or 0
    local is_error = exit_code ~= 0

    if is_error then
      if combined == "" then
        return no_matches_result(result_mode)
      end
      return { content = slice(combined, offset, head_limit), is_error = true }
    end

    if combined == "" then
      return no_matches_result(result_mode)
    end
    return grep_result(slice(combined, offset, head_limit), count_source, result_mode)
  end,
}, { default_ms = 30000, max_ms = 120000, grace_ms = 5000 }))
