-- Input prediction plugin. Predicts the user's next message via a background
-- LLM call and renders it as the prompt's placeholder. Tab accepts; Esc and
-- Ctrl-C dismiss; typing hides it without destroying it, so an undo back to
-- an empty buffer brings it back.
--
-- The system prompt carries the stable instruction; messages carry the recent
-- conversation. Consecutive calls share the KV cache prefix up to the last
-- common message.

local prompt = smelt.prompt.win()

local function clear_prediction()
  prompt:clear_placeholder()
end

local SYSTEM = "Task: predict what the user will type next in the conversation below. Keep it short - one sentence max. If you cannot predict, reply with an empty string."

local function has_queued_messages()
  return #smelt.prompt.queued() > 0
end

-- Accumulated context sent so far. The system prompt is stable,
-- so only the messages array is compared between calls.
local sent_messages = {}

smelt.signal("input_epoch"):subscribe(clear_prediction)

smelt.signal("session_epoch"):subscribe(function()
  clear_prediction()
  sent_messages = {}
end)

smelt.signal("history_epoch"):subscribe(function()
  clear_prediction()
  sent_messages = {}
end)

smelt.events.on("turn_end", function(payload)
  if payload.cancelled then
    return
  end

  prompt:clear_placeholder()

  if has_queued_messages() then
    return
  end

  local history = smelt.session.conversation()

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

  -- Build messages from recent semantic conversation context. Internal session
  -- notes are excluded by smelt.session.conversation().
  local messages = {}
  for _, msg in ipairs(user_msgs) do
    local text = msg.content or ""
    text = smelt.text.truncate(text, 500, { keep = "tail" })
    table.insert(messages, { role = "user", content = text })
  end
  if last_assistant then
    local text = last_assistant.content or ""
    text = smelt.text.truncate(text, 500, { keep = "tail" })
    table.insert(messages, { role = "assistant", content = text })
  end

  -- Skip if nothing changed since last call.
  local changed = #messages ~= #sent_messages
  if not changed then
    for i = 1, #messages do
      if messages[i].role ~= sent_messages[i].role or messages[i].content ~= sent_messages[i].content then
        changed = true
        break
      end
    end
  end
  if not changed then return end
  sent_messages = messages

  local guard = smelt.lifecycle.guard({ "session", "history", "input" }):latest("predict")

  smelt.engine.ask({
    system = SYSTEM,
    messages = messages,
    model = smelt.model.preferred("predict"),
    reasoning_effort = "off",
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
