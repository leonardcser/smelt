-- Built-in load_skill tool. Fetches a skill body via `smelt.skills.content`.

smelt.transcript.register_tool("load_skill", {
  cache_key = "smelt.tool-presentation.load_skill:v1",
  compact = function(block)
    return tostring((block.output and block.output.content_lines) or 0) .. " lines"
  end,
})

smelt.tools.register({
  name = "load_skill",
  description = "Load a skill by name to get specialized instructions and knowledge. Use this when a task matches one of the available skills listed in the system prompt.",
  override = true,
  effect = "read",
  parameters = {
    type = "object",
    properties = {
      name = {
        type = "string",
        description = "The name of the skill to load",
      },
    },
    required = { "name" },
  },
  summary = function(args)
    return args.name or ""
  end,
  execute = function(args)
    local name = args.name or ""
    if name == "" then
      return { content = "missing required parameter: name", is_error = true }
    end
    local content, err = smelt.skills.content(name)
    if content then
      return content
    end
    return { content = err or "skill not found", is_error = true }
  end,
})
