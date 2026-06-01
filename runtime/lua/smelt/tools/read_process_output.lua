-- Built-in read_process_output tool. Lets the agent inspect background bash processes.

local DEFAULT_TIMEOUT_MS = 30000
local MAX_TIMEOUT_MS = 600000
local POLL_MS = 100

local function is_empty(t)
  return t == nil or next(t) == nil
end

local function format_result(r)
  local status
  if r.running then
    status = "running"
  elseif r.exit_code ~= nil then
    status = "exited (code " .. tostring(r.exit_code) .. ")"
  else
    status = "exited"
  end

  local text = r.text or ""
  if text == "" then
    return "[" .. status .. "]"
  end
  return text .. "\n[" .. status .. "]"
end

smelt.tools.register({
  name = "read_process_output",
  description = "Read buffered output from a background bash process by id without draining it. Can optionally wait for the process to finish.",
  override = true,
  elapsed_visible = true,
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  parameters = {
    type = "object",
    properties = {
      id = { type = "string", description = "Background process id (usually the child pid returned by bash), e.g. 12345" },
      wait = { type = "boolean", description = "Wait for the process to finish before returning (default: false)" },
      timeout_ms = { type = "integer", description = "Max wait time in milliseconds when wait=true (default: 30000, max: 600000)" },
    },
    required = { "id" },
  },
  summary = function(args)
    return args.id or ""
  end,
  render = function(_, output)
    if output.is_error then
      return smelt.layout.text(output.content, { hl_group = "ErrorMsg" })
    end
    return smelt.layout.text(output.content or "")
  end,
  execute = function(args)
    local id = args.id or ""
    if id == "" then
      return { content = "missing required parameter: id", is_error = true }
    end

    local wait = args.wait == true
    local timeout_ms = math.min(args.timeout_ms or DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS)
    local elapsed = 0

    while true do
      local r = smelt.process.output(id)
      if is_empty(r) then
        return { content = "no process with id '" .. id .. "'", is_error = true }
      end
      if not wait or not r.running then
        return format_result(r)
      end
      if elapsed >= timeout_ms then
        return format_result(r)
      end
      smelt.sleep(POLL_MS)
      elapsed = elapsed + POLL_MS
    end
  end,
})
