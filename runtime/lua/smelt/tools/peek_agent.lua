-- Built-in peek_agent tool — non-intrusively inspect another agent's
-- knowledge by running a question against their conversation context.
-- Composes `smelt.agent.{find_by_id,send_query,my_id}` over
-- `engine::registry` + `engine::socket`. Mirrors the retired Rust
-- `PeekAgentTool`: target unaware, third-person framing, factual.

if not smelt.engine.multi_agent() then
  return
end

local FRAMING = "Another agent is inspecting this agent's context. "
  .. "Answer the following question factually based on what this agent "
  .. "has done and knows. Answer in third person (\"the agent has...\"), "
  .. "not as the agent itself. Report only what has been done and "
  .. "what is known.\n\n"

smelt.tools.register({
  name = "peek_agent",
  description = "Non-intrusively inspect another agent's knowledge by running a question against their conversation context. The target agent is unaware of the query. Returns an answer synthesized from their context. Use this to understand what another agent knows or has done without interrupting their work.",
  override = true,
  parameters = {
    type = "object",
    properties = {
      target = {
        type = "string",
        description = 'Agent name to query (e.g. "cedar")',
      },
      question = {
        type = "string",
        description = "The question to answer from the target's context",
      },
    },
    required = { "target", "question" },
  },
  execute = function(args)
    local target = args.target or ""
    local question = args.question or ""
    if target == "" then
      return { content = "Missing required parameter: target", is_error = true }
    end
    if question == "" then
      return { content = "Missing required parameter: question", is_error = true }
    end
    local entry = smelt.agent.find_by_id(target)
    if not entry then
      return { content = target .. ": not found", is_error = true }
    end
    local answer, err = smelt.agent.send_query(entry.socket_path, smelt.agent.my_id(), FRAMING .. question)
    if err then
      return { content = target .. ": " .. err, is_error = true }
    end
    return answer
  end,
})
