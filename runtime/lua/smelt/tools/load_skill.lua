-- Built-in load_skill tool — fetch the formatted body of a skill
-- discovered at startup. Composes `smelt.skills.content` (FFI into
-- the shared `SkillLoader`) so the prompt-section listing in the
-- system prompt and this tool's lookups stay in sync.

smelt.tools.register({
  name = "load_skill",
  description = "Load a skill by name to get specialized instructions and knowledge. Use this when a task matches one of the available skills listed in the system prompt.",
  override = true,
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
  confirm_text = function(args)
    return args.name or ""
  end,
  execute = function(args)
    local name = args.name or ""
    if name == "" then
      return { content = "Missing required parameter: name", is_error = true }
    end
    local content, err = smelt.skills.content(name)
    if content then
      return content
    end
    return { content = err or "skill not found", is_error = true }
  end,
})
