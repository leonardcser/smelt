-- Built-in `bash` tool.

local M = {}
local transcript_defaults = require("smelt.transcript.defaults")

local DEFAULT_TIMEOUT_MS = 120000
local MAX_TIMEOUT_MS = 600000
local DEFAULT_BACKGROUND_ON_TIMEOUT = true

-- Read-only command prefixes that auto-approve. Also used locally to avoid
-- suggesting patterns that are already permanently allowed.
local DEFAULT_ALLOW = {
  -- Directory listing & file search
  "ls *", "find *", "tree *",
  -- Text viewing
  "cat *", "head *", "tail *", "less *",
  -- Text search & processing (read-only)
  "grep *", "sort *", "uniq *", "wc *", "diff *", "tr *", "cut *", "jq *",
  -- Path & file info
  "echo *", "pwd *", "which *", "dirname *", "basename *", "realpath *",
  "stat *", "file *", "test *",
  -- Disk & system info
  "du *", "df *", "date *", "whoami *",
  -- Binary inspection
  "sha256sum *", "md5sum *", "xxd *", "hexdump *", "strings *",
}

local DEFAULT_ALLOW_SET = {}
for _, p in ipairs(DEFAULT_ALLOW) do DEFAULT_ALLOW_SET[p] = true end

local function basename(s)
  return s:match("([^/]+)$") or s
end

local function format_duration(secs)
  if secs < 60 then
    return string.format("%ds", secs)
  elseif secs < 3600 then
    return string.format("%dm %ds", secs // 60, secs % 60)
  else
    local h = secs // 3600
    local rest = secs % 3600
    return string.format("%dh %dm %ds", h, rest // 60, rest % 60)
  end
end

local function format_timeout(ms)
  return format_duration(math.floor(ms / 1000))
end

function M.approval_patterns(args)
  local cmd = args.command or ""
  local subs = smelt.shell.split(cmd)
  local patterns = {}
  local seen = {}
  for _, sub in ipairs(subs) do
    local bin = sub:match("^%s*(%S+)") or ""
    local base = basename(bin)
    if base ~= "" and base ~= "cd" then -- cd is a path permission, not a command
      local pat = base .. " *"
      if not DEFAULT_ALLOW_SET[pat] and not seen[pat] then
        seen[pat] = true
        table.insert(patterns, pat)
      end
    end
  end
  return patterns
end

function M.execute(args, ctx)
  local command = args.command or ""

  local err = smelt.shell.check_interactive(command)
  if err then
    return { content = err, is_error = true }
  end
  err = smelt.shell.check_background_op(command)
  if err then
    return { content = err, is_error = true }
  end

  local background = args.background == true
  local background_on_timeout = args.background_on_timeout
  if background_on_timeout == nil then background_on_timeout = DEFAULT_BACKGROUND_ON_TIMEOUT end

  local timeout_ms = args.timeout_ms or DEFAULT_TIMEOUT_MS
  if timeout_ms > MAX_TIMEOUT_MS then
    timeout_ms = MAX_TIMEOUT_MS
  end

  if background then
    local proc_id = smelt.process.spawn_bg(command)
    return {
      content = "started background process " .. proc_id,
      is_error = false,
      metadata = { background_id = proc_id },
    }
  end

  local id = smelt.task.alloc()
  smelt.process.run_streaming(id, ctx.call_id or "", command, timeout_ms, background_on_timeout)
  local result = smelt.task.wait(id)
  return {
    content = result.content or "",
    is_error = result.is_error and true or false,
    metadata = result.background_id and { background_id = result.background_id } or nil,
  }
end

transcript_defaults.__tool_body_renderers.bash = function(block, ctx)
  local output = block.output or { content = "", is_error = false }
  local content = (output.content or ""):gsub("%s+$", "")
  if not content:match("%S") then return nil end
  return transcript_defaults.render_tool_output({ content = content, is_error = output.is_error }, ctx)
end

transcript_defaults.__tool_header_rest_prefixes.bash = {
  { text = "  ", selectable = false, dim = true },
}

smelt.tools.register(smelt.tools._with_watchdog({
  name = "bash",
  override = true,
  default_allow = DEFAULT_ALLOW,
  subpattern_parser = "shell",
  description =
  "Execute a non-interactive bash command and return its output. Commands time out after 2 minutes by default (configurable up to 10 minutes); by default, a still-running command is moved to the background on timeout. Use background=true to start it in the background immediately. When a command is in the background, use read_process_output to inspect it and stop_process to kill it. Do not poll a background command with read_process_output; you will be notified automatically when it completes. Do not use shell backgrounding (`&`) in the command string. For commands expected to produce very long output (e.g. test runners, build logs), pipe the output through `head`, `tail`, or `grep` to keep only the relevant parts and reduce context usage. The shell working directory does not persist between calls. `cd` inside a command is local to that invocation. Prefer absolute paths to avoid relying on the current directory. Do not run interactive commands (editors, pagers, interactive rebases, etc.); they will hang. If there is no non-interactive alternative, ask the user to run it themselves.",
  parameters = {
    type = "object",
    properties = {
      command = { type = "string", description = "Shell command to execute" },
      description = { type = "string", description = "Short (max 10 words) description of what this command does" },
      timeout_ms = { type = "integer", description = "Timeout in milliseconds (default: 120000, max: 600000)" },
      background = { type = "boolean", description = "Run the command in the background and return immediately (default: false)" },
      background_on_timeout = { type = "boolean", description = "If the timeout expires, keep the command running in the background instead of killing it (default: true)" },
    },
    required = { "command" },
  },
  approval_patterns = M.approval_patterns,
  -- `summary` doubles as both the transcript header and the confirm dialog body.
  -- Returning styled-lines (same shape as `buf:styled(lines)`) lets us
  -- syntax-highlight the command without the renderer hard-coding bash.
  summary = function(args)
    local cmd = args.command or ""
    if cmd == "" then return nil end
    local timeout_ms = args.timeout_ms or DEFAULT_TIMEOUT_MS
    if timeout_ms > MAX_TIMEOUT_MS then timeout_ms = MAX_TIMEOUT_MS end
    local lines = {}
    for line in (cmd .. "\n"):gmatch("([^\n]*)\n") do
      local spans = { { text = line, syntax = "bash" } }
      if #lines == 0 then
        local suffix = args.background and "(background)" or ("(timeout: " .. format_timeout(timeout_ms) .. ")")
        spans[#spans + 1] = {
          text = suffix,
          selectable = false,
          title_suffix = true,
          style = { dim = true },
        }
      end
      lines[#lines + 1] = spans
    end
    return lines
  end,
  execute = M.execute,
}, { default_ms = DEFAULT_TIMEOUT_MS, max_ms = MAX_TIMEOUT_MS, grace_ms = 5000 }))

return M
