-- Built-in `bash` tool.

local M = {}
local transcript_defaults = require("smelt.transcript.defaults")
local detach = require("smelt._detach")

local DEFAULT_TIMEOUT_MS = 120000
local MAX_TIMEOUT_MS = 600000
local DEFAULT_BACKGROUND_ON_TIMEOUT = true

-- Models sometimes send numbers and booleans as strings ("30000", "true").
local function num_arg(v, default)
  local n = tonumber(v)
  if n == nil or n ~= n then return default end
  return math.floor(n)
end

local function bool_arg(v, default)
  if type(v) == "boolean" then return v end
  if v == "true" then return true end
  if v == "false" then return false end
  return default
end

local function timeout_arg(v)
  return math.min(math.max(num_arg(v, DEFAULT_TIMEOUT_MS), 1), MAX_TIMEOUT_MS)
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

local LEVEL = { read = 1, write = 2, full = 3 }

-- The sandbox a command runs in: the mode's own level, so the command runs
-- without asking, or more when the model asks for it. With no sandbox on
-- this machine, every command runs unconfined and counts as "full".
function M.access(args, mode)
  local access = mode.access
  if LEVEL[args.access] and LEVEL[args.access] > LEVEL[access] then access = args.access end
  if access ~= "full" and not smelt.sandbox.status().available then return "full" end
  return access
end

-- Errors a command gets when the sandbox blocks a path.
local SANDBOX_ERRORS = { "Permission denied", "Read-only file system", "Operation not permitted" }

-- A failed sandboxed command that reports a blocked path gets a note, so
-- the model asks for more access instead of working around the error.
local function sandbox_hint(access, content)
  if access == "full" then return nil end
  for _, err in ipairs(SANDBOX_ERRORS) do
    if content:find(err, 1, true) then
      if access == "read" then
        return "This command ran in the read sandbox, which may write only temp directories "
          .. "and cannot see protected paths such as credentials. If the sandbox caused the "
          .. "error, run it again with access = \"write\" (workspace, temp directories and "
          .. "tool caches) or \"full\" (protected paths and anywhere else)."
      end
      return "This command ran in the write sandbox, which may write only the workspace, "
        .. "temp directories and tool caches, and cannot see protected paths such as "
        .. "credentials. If the sandbox caused the error, run it again with access = \"full\"."
    end
  end
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

  local background = bool_arg(args.background, false)
  local background_on_timeout = bool_arg(args.background_on_timeout, DEFAULT_BACKGROUND_ON_TIMEOUT)
  local notify = bool_arg(args.notify_on_completion, true)
  if not notify and not background then
    return { content = "notify_on_completion=false requires background=true", is_error = true }
  end

  local timeout_ms = timeout_arg(args.timeout_ms)
  local access = M.access(args, smelt.mode.get(ctx.mode))

  if background then
    local job_id = smelt.process.spawn_bg(command, { notify_on_completion = notify })
    return {
      content = "started background process " .. job_id
        .. (notify and ", you'll be notified when it completes" or ", completion notification disabled"),
      is_error = false,
      metadata = { background = true, background_id = job_id },
    }
  end

  local id = smelt.task.alloc()
  smelt.process.run_streaming(id, ctx.call_id or "", command, {
    timeout_ms = timeout_ms,
    background_on_timeout = background_on_timeout,
  })
  local result = smelt.task.wait(id)
  local metadata
  if result.background_id or result.termination then
    metadata = {
      background = result.background_id and true or nil,
      background_id = result.background_id,
      termination = result.termination,
    }
  end
  local content = result.content or ""
  local hint = result.is_error and sandbox_hint(access, content)
  if hint then content = content .. "\n\n" .. hint end
  return {
    content = content,
    is_error = result.is_error and true or false,
    metadata = metadata,
  }
end

smelt.transcript.register_tool("bash", {
  compact = function(block)
    return detach.waiting_hint(block)
  end,
  body = function(block, ctx)
    local items = {}
    local output = block.output
    if output and (tonumber(output.content_bytes) or 0) > 0 then
      items[#items + 1] = transcript_defaults.render_tool_output(output, ctx)
    end
    local hint = detach.waiting_hint(block)
    if hint then items[#items + 1] = smelt.layout.gutter(hint, { text = "  " }) end
    if #items == 0 then return nil end
    return smelt.layout.vbox(items)
  end,
})

smelt.tools.register(__smelt_internal.tools.with_watchdog({
  name = "bash",
  override = true,
  access = M.access,
  -- The sandbox confines every write, so the command names no paths.
  paths = function() return {} end,
  description =
  "Run non-interactive bash and return output. Commands run in an OS sandbox: with access=read they can write only temp directories; with access=write they can also write the workspace and tool caches; neither can see protected paths such as credentials; access=full runs without a sandbox and may need user approval. Leave access unset to use the most the current mode allows without asking. Background jobs automatically notify on completion by default, even after your turn. Do not sleep or poll just to wait; do other work, or end your turn if nothing is ready. Use read_process_output to inspect output and stop_process to stop jobs. Avoid broad filename-based cleanup with pkill -f or killall: it can kill smelt or unrelated processes. For processes not managed by this tool, identify and verify the exact PID before stopping it. Do not use shell backgrounding (`&`). Pipe long output (e.g. test or build logs) through head, tail, or grep to retain relevant parts and save context. The working directory is not preserved between calls; cd affects only that invocation. Prefer absolute paths. Do not run interactive commands (editors, pagers, interactive rebases): they hang. Ask the user to run them if no non-interactive alternative exists.",
  parameters = {
    type = "object",
    properties = {
      command = { type = "string", description = "Shell command to execute." },
      description = { type = "string", description = "Command summary (max 10 words)." },
      timeout_ms = { type = "integer", description = "Timeout in milliseconds; default: 120000; max: 600000." },
      background = { type = "boolean", description = "Start in background; return immediately. Default: false." },
      notify_on_completion = { type = "boolean", description = "For background=true, notify the agent and keep the session waiting until completion (default: true). Set false for detached servers or watchers." },
      background_on_timeout = { type = "boolean", description = "On timeout, keep running in background instead of killing. Default: true." },
      access = { type = "string", enum = { "read", "write", "full" }, description = "Sandbox level to ask for when the command needs more than the current mode allows without asking." },
    },
    required = { "command" },
  },
  -- `summary` doubles as both the transcript header and the confirm dialog body.
  -- Returning styled-lines (same shape as `buf:styled(lines)`) lets us
  -- syntax-highlight the command without the renderer hard-coding bash.
  summary = function(args)
    local cmd = args.command or ""
    if cmd == "" then return nil end
    local timeout_ms = timeout_arg(args.timeout_ms)
    local lines = {}
    for line in (cmd .. "\n"):gmatch("([^\n]*)\n") do
      local spans = { { text = line, syntax = "bash" } }
      if #lines == 0 then
        -- The permission dialog says the command will run in the background.
        spans[#spans + 1] = bool_arg(args.background, false)
            and { text = "background", fg = "SmeltProcess", selectable = false, title_suffix = true }
          or {
            text = "(timeout: " .. format_timeout(timeout_ms) .. ")",
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
