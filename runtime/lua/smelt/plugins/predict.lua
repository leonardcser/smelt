-- Input prediction plugin. Predicts the user's next message via a background
-- LLM call and renders it as the prompt's placeholder. Tab accepts; Esc and
-- Ctrl-C dismiss; typing hides it without destroying it, so an undo back to
-- an empty buffer brings it back.

local aux = require("smelt.aux")
local prompt = smelt.prompt.win()

smelt.cell("turn_end"):subscribe(function(payload)
  if payload.cancelled then
    return
  end

  prompt:clear_placeholder()

  local history = smelt.session.messages()

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

  local parts = {}
  for _, msg in ipairs(user_msgs) do
    local text = msg.content or ""
    text = smelt.text.truncate(text, 500, { keep = "tail" })
    table.insert(parts, "User: " .. text)
  end
  if last_assistant then
    local text = last_assistant.content or ""
    text = smelt.text.truncate(text, 500, { keep = "tail" })
    table.insert(parts, "Assistant: " .. text)
  end

  local question = "Recent conversation:\n\n"
    .. table.concat(parts, "\n\n")
    .. "\n\nTask: predict what the user will type next in the conversation above. Keep it short — one sentence max. If you cannot predict, reply with an empty string."

  smelt.engine.ask({
    system = aux.SYSTEM,
    question = question,
    model = smelt.model.preferred("predict"),
    on_response = function(content, err)
      if err then return end
      -- Keep only the first line; `Win:placeholder` rejects newlines and
      -- the prompt only renders a single line of ghost text anyway.
      local text = (content:match("[^\n]+") or ""):match("^%s*(.-)%s*$")
      if text ~= "" then
        prompt:placeholder(text, { accept_keys = { "tab" } })
      end
    end,
  })
end)
