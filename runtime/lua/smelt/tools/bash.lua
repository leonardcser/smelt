-- Built-in `bash` tool. Composes shell-validation helpers
-- (`smelt.shell.{check_interactive,check_background_op,split,
-- is_default_bash_allow}`) with the streaming subprocess primitive
-- `smelt.process.run_streaming` — the latter spawns `sh -c command`
-- on a tokio task, fires `EngineEvent::ToolOutput` per stdout/stderr
-- line as the child runs, and resumes this coroutine with the
-- aggregated result on exit.

local M = {}

local DEFAULT_TIMEOUT_MS = 120000
local MAX_TIMEOUT_MS = 600000

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

function M.approval_patterns(args)
  local cmd = args.command or ""
  local subs = smelt.shell.split(cmd)
  local patterns = {}
  local seen = {}
  for _, sub in ipairs(subs) do
    local bin = sub:match("^%s*(%S+)") or ""
    local base = basename(bin)
    -- `cd` is a path permission, not a command permission.
    if base ~= "" and base ~= "cd" then
      local pat = base .. " *"
      if not smelt.shell.is_default_bash_allow(pat) and not seen[pat] then
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

  local timeout_ms = args.timeout_ms or DEFAULT_TIMEOUT_MS
  if timeout_ms > MAX_TIMEOUT_MS then
    timeout_ms = MAX_TIMEOUT_MS
  end

  local id = smelt.task.alloc()
  smelt.process.run_streaming(id, ctx.call_id or "", command, timeout_ms)
  local result = smelt.task.wait(id)
  return {
    content = result.content or "",
    is_error = result.is_error and true or false,
  }
end

smelt.tools.register({
  name = "bash",
  override = true,
  elapsed_visible = true,
  description =
  "Execute a non-interactive bash command and return its output. The working directory persists between calls. Commands time out after 2 minutes by default (configurable up to 10 minutes). Do not use shell backgrounding (`&`) in the command string. Do not run interactive commands (editors, pagers, interactive rebases, etc.) — they will hang. If there is no non-interactive alternative, ask the user to run it themselves.",
  parameters = {
    type = "object",
    properties = {
      command = { type = "string", description = "Shell command to execute" },
      description = { type = "string", description = "Short (max 10 words) description of what this command does" },
      timeout_ms = { type = "integer", description = "Timeout in milliseconds (default: 120000, max: 600000)" },
    },
    required = { "command" },
  },
  needs_confirm = function(args) return args.command or "" end,
  approval_patterns = M.approval_patterns,
  summary = function(args)
    local d = args.description or ""
    return d ~= "" and d or nil
  end,
  render = function(args, output, width, buf)
    smelt.text.render(buf, output.content, { is_error = output.is_error })
  end,
  render_summary = function(buf, line, args)
    smelt.bash.render_line(buf, line)
  end,
  header_suffix = function(args, ctx)
    if ctx.status ~= "pending" then return nil end
    local ms = args.timeout_ms or DEFAULT_TIMEOUT_MS
    local secs = math.floor(ms / 1000)
    return "timeout: " .. format_duration(secs)
  end,
  execute = M.execute,
})

return M
