-- Built-in stop_process tool. Stops a background bash process by id.

local function is_empty(t)
  return t == nil or next(t) == nil
end

local function format_output(prefix, text)
  text = text or ""
  if text == "" then
    return prefix .. " (no output)"
  end
  return prefix .. "\n" .. text
end

smelt.tools.register({
  name = "stop_process",
  description = "Stop a running background bash process by id and return its buffered output.",
  override = true,
  elapsed_visible = true,
  effect = "process_control",
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
    return smelt.layout.tool_output(output, ctx)
  end,
  execute = function(args)
    local id = args.id or ""
    if id == "" then
      return { content = "missing required parameter: id", is_error = true }
    end

    local before = smelt.process.output(id)
    if is_empty(before) then
      return { content = "no process with id '" .. id .. "'", is_error = true }
    end
    if before.running == false then
      return format_output("process already exited", before.text)
    end

    local result, err = smelt.process.stop(id)
    if err then
      return { content = err, is_error = true }
    end
    return format_output("process stopped", result and result.text)
  end,
})
