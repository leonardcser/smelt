-- Built-in spawn_agent tool — spawn a subagent for a scoped task.
-- Composes `smelt.agent.{spawn,wait_for_message,subagent_meta,list,my_pid}`
-- (FFI over `EngineHandle::spawn_subagent` + the `AgentMessageNotification`
-- broadcast). Mirrors the retired Rust `SpawnAgentTool`: max-agents cap,
-- spawn the binary, optional 10-minute blocking wait, formatted result.

if not smelt.engine.multi_agent() then
  return
end

local BLOCKING_TIMEOUT_MS = 600000

smelt.tools.register({
  name = "spawn_agent",
  description = "Spawn a new subagent to work on a task. The subagent runs with full tool access. Give it a well-scoped task with all the context it needs — relevant files, constraints, and how its work fits into the larger picture. Set `wait` to true to block until the agent finishes and get its result directly. Subagents persist and build context — reuse them for related follow-ups via `message_agent`.",
  override = true,
  parameters = {
    type = "object",
    properties = {
      prompt = {
        type = "string",
        description = "Detailed instructions for the subagent. Include task description, relevant files, constraints, and any context needed.",
      },
      wait = {
        type = "boolean",
        description = "If true, block until the subagent finishes and return its result. If false (default), spawn in the background and continue immediately.",
      },
    },
    required = { "prompt" },
  },
  execute = function(args)
    local prompt = args.prompt or ""
    local blocking = args.wait == true

    local meta = smelt.agent.subagent_meta()
    if not meta then
      return { content = "multi-agent disabled", is_error = true }
    end

    local my_pid = smelt.agent.my_pid()
    local children = smelt.agent.list()
    if #children >= meta.max_agents then
      return {
        content = "cannot spawn: already at max agents (" .. meta.max_agents
                  .. ") for this session",
        is_error = true,
      }
    end

    local agent_id, err = smelt.agent.spawn(prompt, blocking, smelt.session.dir())
    if err then
      return { content = "failed to spawn subagent: " .. err, is_error = true }
    end

    if not blocking then
      return {
        content = "agent " .. agent_id .. " is now working in the background",
        metadata = { agent_id = agent_id, blocking = false },
      }
    end

    local task_id = smelt.task.alloc()
    smelt.agent.wait_for_message(task_id, agent_id, my_pid, BLOCKING_TIMEOUT_MS)
    local result = smelt.task.wait(task_id)
    if result.error then
      return { content = "agent " .. agent_id .. ": " .. result.error, is_error = true }
    end
    return {
      content = "agent " .. agent_id .. " finished:\n" .. (result.message or ""),
      metadata = { agent_id = agent_id, blocking = true },
    }
  end,
})
