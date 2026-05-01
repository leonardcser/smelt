-- Built-in message_agent tool — send a message to one or more
-- agents. Composes `smelt.agent.{find_by_id,send_message,my_id,my_slug}`
-- over `engine::registry` + `engine::socket`. Mirrors the retired Rust
-- `MessageAgentTool`: fire-and-forget, partial delivery reported.

if not smelt.engine.multi_agent() then
  return
end

smelt.tools.register({
  name = "message_agent",
  description = "Send a message to one or more agents. Use the agent name from `list_agents` or from the <agent-message from=\"name\"> tag. The recipient may be busy and reply later. Use this to steer subagents, provide information, or coordinate work.",
  override = true,
  parameters = {
    type = "object",
    properties = {
      targets = {
        type = "array",
        items = { type = "string" },
        description = 'List of agent names (slugs) to send the message to. Use the name from the <agent-message from="name"> tag. Example: ["reed", "plum"]',
      },
      message = {
        type = "string",
        description = "The message to send",
      },
    },
    required = { "targets", "message" },
  },
  execute = function(args)
    local message = args.message or ""
    local targets = args.targets or {}
    if type(targets) ~= "table" or #targets == 0 then
      return { content = "no targets specified", is_error = true }
    end

    local my_id = smelt.agent.my_id()
    local my_slug = smelt.agent.my_slug()

    local delivered = {}
    local errors = {}

    for _, id in ipairs(targets) do
      local entry = smelt.agent.find_by_id(id)
      if not entry then
        table.insert(errors, id .. ": not found")
      else
        local _, err = smelt.agent.send_message(entry.socket_path, my_id, my_slug, message)
        if err then
          table.insert(errors, id .. ": " .. err)
        else
          table.insert(delivered, id)
        end
      end
    end

    if #errors == 0 then
      return "delivered"
    elseif #delivered == 0 then
      return { content = "failed: " .. table.concat(errors, "; "), is_error = true }
    else
      local list = "[" .. table.concat(
        (function()
          local q = {}
          for _, d in ipairs(delivered) do
            table.insert(q, '"' .. d .. '"')
          end
          return q
        end)(),
        ", "
      ) .. "]"
      return "partial: delivered to " .. list .. ", failed: " .. table.concat(errors, "; ")
    end
  end,
})
