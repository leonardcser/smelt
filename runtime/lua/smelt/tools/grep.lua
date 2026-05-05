-- Built-in grep tool — ripgrep-first, with a `grep` fallback when
-- `rg` is missing. Migrated from `engine::tools::grep` to compose
-- `tui::grep` + `tui::process` via FFI.

local function pick_bool(v, default)
  if type(v) == "boolean" then return v end
  return default
end

local function pick_int(v, default)
  if type(v) == "number" then return math.floor(v) end
  return default or 0
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

local function run_rg(args)
  local pattern = args.pattern or ""
  local path = args.path or ""
  local mode = args.output_mode
  if mode == nil or mode == "" then mode = "content" end

  local context = pick_int(args.context, 0)
  if context == 0 then context = pick_int(args["-C"], 0) end

  local glob_filter = args.glob
  if glob_filter == "" then glob_filter = nil end
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
    timeout_secs = math.max(1, math.floor(timeout_ms / 1000)),
  }
  return smelt.grep.run(pattern, path, opts)
end

local function run_grep_fallback(args)
  local pattern = args.pattern or ""
  local search_path = args.path or ""
  if search_path == "" then search_path = "." end
  local case_insensitive = pick_bool(args["-i"], false)
  local glob_filter = args.glob
  local timeout_ms = pick_int(args.timeout_ms, 30000)
  if timeout_ms <= 0 then timeout_ms = 30000 end

  local cmd_args = { "-rn", "--max-count=200" }
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

smelt.tools.register({
  name = "grep",
  description = "A powerful search tool built on ripgrep. Supports full regex syntax, file type filtering, glob filtering, and multiple output modes.",
  override = true,
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
        description = 'Output mode: "content" shows matching lines (default), "files_with_matches" shows file paths, "count" shows match counts.',
      },
      ["-i"] = { type = "boolean", description = "Case insensitive search (rg -i)" },
      ["-n"] = { type = "boolean", description = 'Show line numbers in output (rg -n). Requires output_mode: "content", ignored otherwise. Defaults to true.' },
      ["-A"] = { type = "integer", description = 'Number of lines to show after each match (rg -A). Requires output_mode: "content", ignored otherwise.' },
      ["-B"] = { type = "integer", description = 'Number of lines to show before each match (rg -B). Requires output_mode: "content", ignored otherwise.' },
      ["-C"] = { type = "integer", description = "Alias for context." },
      context = { type = "integer", description = 'Number of lines to show before and after each match. Only applies to output_mode "content".' },
      multiline = { type = "boolean", description = "Enable multiline mode where . matches newlines and patterns can span lines." },
      head_limit = { type = "integer", description = "Limit output to first N lines/entries. 0 means unlimited (default)." },
      offset = { type = "integer", description = "Skip first N lines/entries before applying head_limit." },
      timeout_ms = { type = "integer", description = "Timeout in milliseconds (default: 30000)" },
    },
    required = { "pattern" },
  },
  needs_confirm = function(args)
    local pattern = args.pattern or ""
    local path = args.path or ""
    if path == "" then return pattern end
    return pattern .. " in " .. path
  end,
  render = function(args, output, width, buf)
    local content = output.content or ""
    local n = 0
    if content ~= "" then
      local _, newlines = content:gsub("\n", "\n")
      n = newlines
      if content:sub(-1) ~= "\n" then
        n = n + 1
      end
    end
    smelt.text.render(buf, n .. " matches")
  end,
  paths_for_workspace = function(args)
    local p = args.path or ""
    return p ~= "" and { p } or {}
  end,
  execute = function(args)
    local offset = pick_int(args.offset, 0)
    local head_limit = pick_int(args.head_limit, 0)

    local out, err = run_rg(args)
    if not out then
      out, err = run_grep_fallback(args)
      if not out then
        return { content = err or "grep failed", is_error = true }
      end
    end

    local combined = combine_streams(out.stdout, out.stderr)
    if out.timed_out then
      local secs = math.floor(((args.timeout_ms or 30000) / 1000) + 0.5)
      return { content = string.format("timed out after %ds", secs), is_error = true }
    end

    local exit_code = out.exit_code or 0
    local is_error = exit_code ~= 0

    if is_error then
      if combined == "" then
        return "no matches found"
      end
      return { content = slice(combined, offset, head_limit), is_error = true }
    end

    if combined == "" then
      return "no matches found"
    end
    return slice(combined, offset, head_limit)
  end,
})
