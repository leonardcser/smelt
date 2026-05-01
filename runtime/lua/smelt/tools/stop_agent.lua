-- Built-in stop_agent tool — kill a subagent and all its children.
-- Composes `smelt.agent.{find_by_id,is_in_tree,kill}` (FFI over
-- `engine::registry::*`). Mirrors the retired Rust `StopAgentTool`:
-- only owners can stop the target; lookup-by-id; trees descend.

if not smelt.engine.multi_agent() then
  return
end

smelt.tools.register({
  name = "stop_agent",
  description = "Stop a subagent and all its children. Only works on agents you own. Use to cancel work that is no longer needed or has been superseded.",
  override = true,
  parameters = {
    type = "object",
    properties = {
      target = {
        type = "string",
        description = 'Agent name to stop (e.g. "cedar")',
      },
    },
    required = { "target" },
  },
  execute = function(args)
    local target = args.target or ""
    if target == "" then
      return { content = "Missing required parameter: target", is_error = true }
    end
    local entry = smelt.agent.find_by_id(target)
    if not entry then
      return { content = target .. " not found", is_error = true }
    end
    local my_pid = smelt.agent.my_pid()
    if not smelt.agent.is_in_tree(entry.pid, my_pid) then
      return { content = target .. " is not owned by you", is_error = true }
    end
    smelt.agent.kill(entry.pid)
    return "stopped " .. target
  end,
})
