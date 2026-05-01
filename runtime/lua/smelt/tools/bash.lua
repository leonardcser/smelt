-- Built-in `bash` tool. Composes shell-validation helpers
-- (`smelt.shell.{check_interactive,check_background_op,split,
-- is_default_bash_allow}`) with the streaming subprocess primitive
-- `smelt.process.run_streaming` — the latter spawns `sh -c command`
-- on a tokio task, fires `EngineEvent::ToolOutput` per stdout/stderr
-- line as the child runs, and resumes this coroutine with the
-- aggregated result on exit.
--
-- `run_in_background=true` short-circuits to `smelt.process.spawn_bg`
-- and returns the registry id. The associated `read_process_output`
-- and `stop_process` tools live in `plugins/background_commands.lua`.

local M = {}

local DEFAULT_TIMEOUT_MS = 120000
local MAX_TIMEOUT_MS = 600000

local function basename(s)
  return s:match("([^/]+)$") or s
end

local function approval_patterns(args)
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

local function execute(args, ctx)
  local command = args.command or ""

  local err = smelt.shell.check_interactive(command)
  if err then
    return { content = err, is_error = true }
  end
  err = smelt.shell.check_background_op(command)
  if err then
    return { content = err, is_error = true }
  end

  if args.run_in_background then
    local ok, id_or_err = pcall(smelt.process.spawn_bg, command)
    if not ok then
      return { content = tostring(id_or_err), is_error = true }
    end
    return "background process started with id: " .. id_or_err
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

local BG_PARAM_DESC =
"Run the command in the background and return a process ID. Use read_process_output to check output and stop_process to kill it."

smelt.tools.register({
  name = "bash",
  override = true,
  description =
  "Execute a non-interactive bash command and return its output. The working directory persists between calls. Commands time out after 2 minutes by default (configurable up to 10 minutes). For long-running processes set run_in_background=true. Do not use shell backgrounding (`&`) in the command string. Do not run interactive commands (editors, pagers, interactive rebases, etc.) — they will hang. If there is no non-interactive alternative, ask the user to run it themselves.",
  parameters = {
    type = "object",
    properties = {
      command = { type = "string", description = "Shell command to execute" },
      description = { type = "string", description = "Short (max 10 words) description of what this command does" },
      timeout_ms = { type = "integer", description = "Timeout in milliseconds (default: 120000, max: 600000)" },
      run_in_background = { type = "boolean", description = BG_PARAM_DESC },
    },
    required = { "command" },
  },
  needs_confirm = function(args) return args.command or "" end,
  approval_patterns = approval_patterns,
  execute = execute,
})

return M
