-- Built-in read_process_output tool. Lets the agent inspect background bash processes.

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
  description = "Read the current buffered output from a background bash process by id without draining or waiting.",
  override = true,
  elapsed_visible = true,
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
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

    local r = smelt.process.output(id)
    if is_empty(r) then
      return { content = "no process with id '" .. id .. "'", is_error = true }
    end
    return format_result(r)
  end,
})
