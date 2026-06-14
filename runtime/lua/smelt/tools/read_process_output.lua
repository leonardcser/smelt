-- Built-in read_process_output tool. Lets the agent inspect background bash processes.

local function is_empty(t)
  return t == nil or next(t) == nil
end

local function format_result(r)
  local text = r.text or ""
  if r.running then
    return text
  end

  local status
  if r.exit_code ~= nil then
    status = "process exited with code " .. tostring(r.exit_code)
  else
    status = "process exited"
  end

  if text == "" then
    return status
  end
  return text .. "\n\n" .. status
end

smelt.tools.register({
  name = "read_process_output",
  description = "Read the captured output snapshot from a background bash process by id without draining or waiting. Running processes return only buffered stdout/stderr, which may be empty; exited processes append the exit status.",
  override = true,
  elapsed_visible = true,
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  effect = "process_read",
  parameters = {
    type = "object",
    properties = {
      id = { type = "string", description = "Background process id (usually the child pid returned by bash), e.g. 12345" },
    },
    required = { "id" },
  },
  summary = function(args)
    return args.id or ""
  end,
  render = function(_, output, ctx)
    return require("smelt.transcript.defaults").render_tool_output(output, ctx)
  end,
  execute = function(args)
    local id = args.id or ""
    if id == "" then
      return { content = "missing required parameter: id", is_error = true }
    end

    local r = smelt.process.output(id)
    if is_empty(r) then
      return { content = "no process with id '" .. id .. "'", is_error = true }
    end
    return format_result(r)
  end,
})
