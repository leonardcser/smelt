-- Built-in list_agents tool — workspace-wide agent registry view.
-- Composes `smelt.agent.discover()` (which prunes dead entries) and
-- formats the result into an aligned table separating "owned" (this
-- agent's descendants) from "peer" entries.
--
-- Only registers when multi-agent is enabled — tracks the engine-side
-- `if let Some(ma) = ma` gate the retired `ListAgentsTool` lived
-- behind, so the LLM doesn't see this tool in single-agent runs.

if not smelt.engine.multi_agent() then
  return
end

local function build_owned_set(entries, my_pid)
  local children = {}
  for _, e in ipairs(entries) do
    local pp = e.parent_pid
    if pp ~= nil then
      children[pp] = children[pp] or {}
      table.insert(children[pp], e.pid)
    end
  end
  local owned = {}
  local stack = { my_pid }
  while #stack > 0 do
    local cur = table.remove(stack)
    local kids = children[cur]
    if kids then
      for _, k in ipairs(kids) do
        owned[k] = true
        table.insert(stack, k)
      end
    end
  end
  return owned
end

smelt.tools.register({
  name = "list_agents",
  description = "List agents in the current workspace with their name, status, task slug, and whether they are owned (your subagents) or peers. Use to discover agent names before calling `message_agent` or `stop_agent`.",
  override = true,
  parameters = {
    type = "object",
    properties = {},
  },
  execute = function(_args)
    local entries = smelt.agent.discover()
    local my_pid = smelt.agent.my_pid()

    local others = {}
    for _, e in ipairs(entries) do
      if e.pid ~= my_pid then
        table.insert(others, e)
      end
    end

    if #others == 0 then
      return "No other agents found."
    end

    local owned = build_owned_set(entries, my_pid)

    local name_w = 0
    for _, e in ipairs(others) do
      if #e.agent_id > name_w then
        name_w = #e.agent_id
      end
    end
    local status_w = 7

    local lines = {}
    for _, e in ipairs(others) do
      local agent_type = owned[e.pid] and "owned" or "peer "
      local status = e.status
      local slug = e.task_slug or ""
      local name = e.agent_id .. string.rep(" ", math.max(0, name_w - #e.agent_id))
      local stat = status .. string.rep(" ", math.max(0, status_w - #status))
      table.insert(lines, name .. "  " .. agent_type .. "  " .. stat .. "  " .. slug)
    end

    return table.concat(lines, "\n")
  end,
})
