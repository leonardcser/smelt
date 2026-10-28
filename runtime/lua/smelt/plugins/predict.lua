-- Input prediction plugin. Predicts the user's next message via a background
-- LLM call and renders it as the prompt's placeholder. Tab accepts; Esc and
-- Ctrl-C dismiss; typing hides it without destroying it, so an undo back to
-- an empty buffer brings it back.
--
-- The system prompt carries the stable instruction. The recent conversation is
-- quoted in a single user message: a request ending on an assistant message
-- would have the model continue the assistant instead of predicting the user.

local prompt = smelt.prompt.win()

local function clear_prediction()
  prompt:clear_placeholder()
end

local SYSTEM = "You predict the next message a developer will type to their coding assistant. "
  .. "The conversation is quoted below; you are not a participant in it. "
  .. "Write only that next message, in the developer's own words: a short instruction, answer, or question addressed to the assistant, "
  .. "such as \"yes, go ahead\" or \"now run the tests\". "
  .. "Never write as the assistant: never offer help, summarize, or ask the developer anything. "
  .. "One short line, no quotes. If the next message is unpredictable, reply with nothing."

local function has_queued_messages()
  return #smelt.prompt.queued() > 0
end

-- The conversation last sent, so an unchanged one is not predicted twice.
local sent_conversation

smelt.signal.subscribe("input_epoch", clear_prediction)

smelt.signal.subscribe("session_epoch", function()
  clear_prediction()
  sent_conversation = nil
end)

smelt.signal.subscribe("history_epoch", function()
  clear_prediction()
  sent_conversation = nil
end)

smelt.events.on("turn_end", function(payload)
  if payload.cancelled or not smelt.settings.show_prediction then
    return
  end

  prompt:clear_placeholder()

  if has_queued_messages() then
    return
  end

  local history = smelt.session.conversation({ limit = 16 })

  local user_msgs = {}
  local last_assistant = nil
  for i = #history, 1, -1 do
    local msg = history[i]
    if msg.role == "user" and #user_msgs < 3 then
      table.insert(user_msgs, 1, msg)
    elseif msg.role == "assistant" and not last_assistant then
      last_assistant = msg
    end
    if #user_msgs >= 3 and last_assistant then
      break
    end
  end

  if #user_msgs == 0 then
    return
  end

  -- Recent semantic conversation context. Internal session notes are
  -- excluded by smelt.session.conversation().
  local turns = {}
  for _, msg in ipairs(user_msgs) do
    table.insert(turns, "Developer: " .. smelt.text.truncate(msg.content or "", 500, { keep = "tail" }))
  end
  if last_assistant then
    table.insert(turns, "Assistant: " .. smelt.text.truncate(last_assistant.content or "", 500, { keep = "tail" }))
  end

  local conversation = table.concat(turns, "\n\n")
  if conversation == sent_conversation then return end
  sent_conversation = conversation

  local guard = smelt.lifecycle.guard({ "session", "history", "input" }):latest("predict")

  smelt.engine.ask({
    system = SYSTEM,
    messages = {
      {
        role = "user",
        content = "<conversation>\n" .. conversation .. "\n</conversation>\n\nThe developer's next message:",
      },
    },
    model = smelt.model.preferred("predict"),
    guard = guard,
    on_response = function(response, err)
      if err then return end
      local content = (response and response.content) or ""
      -- Keep only the first line; `Win:placeholder` rejects newlines and
      -- the prompt only renders a single line of ghost text anyway.
      local text = (content:match("[^\n]+") or ""):match("^%s*(.-)%s*$")
      if text ~= "" and not has_queued_messages() then
        prompt:placeholder(text, { accept_keys = { "tab" } })
      end
    end,
  })
end)
