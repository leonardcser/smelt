-- Built-in /btw command. Asks a side question and renders the answer in
-- a docked dialog. The auxiliary request inherits the live session so it
-- can reuse the same cached prefix as the main conversation.

local MAX_TOOL_CALL_RESTARTS = 2
local TOOL_DENIED_MESSAGE = "Tool use is not allowed for /btw. Respond with text only."
local WAITING_FRAMES = { ".  ", ".. ", "..." }
local WAITING_FRAME_MS = 350

local function build_question(question)
  return "The user is asking a quick side question while working on something else. "
    .. "Answer concisely and directly.\n\n"
    .. "Under no circumstances use tools; any tool call will be denied and you must answer with text only.\n\n"
    .. "Question: " .. question
end

local function clone_messages(messages)
  local out = {}
  for i, msg in ipairs(messages or {}) do
    out[i] = msg
  end
  return out
end

local function build_base_messages(question)
  local out = clone_messages(smelt.session.model_messages())
  table.insert(out, { role = "user", content = build_question(question) })
  return out
end

local function append_tool_denials(messages, assistant_response)
  local out = clone_messages(messages)
  table.insert(out, assistant_response)
  for _, call in ipairs(assistant_response.tool_calls or {}) do
    table.insert(out, {
      role = "tool",
      tool_call_id = call.id,
      content = TOOL_DENIED_MESSAGE,
      is_error = true,
    })
  end
  return out
end

smelt.cmd.register("btw", function(args)
  local question = args or ""
  if question == "" then
    smelt.notify.error("usage: /btw <question>")
    return
  end

  smelt.spawn(function()
    local buf = smelt.buf.new({ readonly = true })
    local stream = smelt.transcript.stream(buf)
    local waiting = false
    local waiting_frame = 0
    local waiting_timer

    local function render_waiting()
      if not waiting then return end
      waiting_frame = waiting_frame % #WAITING_FRAMES + 1
      buf:styled({ {
        { text = WAITING_FRAMES[waiting_frame], style = { dim = true, italic = true } },
      } })
    end

    local function stop_waiting()
      waiting = false
      if waiting_timer then
        waiting_timer:remove()
        waiting_timer = nil
      end
    end

    local function start_waiting()
      stop_waiting()
      waiting = true
      waiting_frame = 0
      render_waiting()
      waiting_timer = smelt.timer.every(WAITING_FRAME_MS, render_waiting)
    end

    local base_messages = build_base_messages(question)
    local restarts = 0

    local function send(messages, retrying_after_tool_denial)
      start_waiting()
      smelt.engine.ask_inherited({
        messages = messages,
        model = smelt.model.preferred("btw"),
        on_delta = function(delta)
          if delta ~= "" then
            stop_waiting()
            stream:append(delta)
          end
        end,
        on_response = function(response, err)
          stop_waiting()
          if err then
            stream:reset()
            buf:source("error (" .. err.kind .. "): " .. err.message)
            return
          end
          if response and response.tool_calls and #response.tool_calls > 0 then
            if not retrying_after_tool_denial then
              stream:reset()
              send(append_tool_denials(messages, response), true)
              return
            end
            if restarts < MAX_TOOL_CALL_RESTARTS then
              restarts = restarts + 1
              stream:reset()
              send(base_messages, false)
              return
            end
            stream:reset()
            buf:source("error (invalid_response): model kept requesting tools")
            return
          end
          stream:finish((response and response.content) or "")
        end,
      })
    end

    send(base_messages, false)

    local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })

    smelt.dialog.open({
      title      = question,
      min_height = "30%",
      max_height = "70%",
      panels     = { { leaf = leaf, height = "fill" } },
      on_close   = stop_waiting,
      close_with_q = true,
    })
  end)
end, { desc = "ask a side question", args = { "<question>" } })
