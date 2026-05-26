-- Built-in /btw command. Asks a side question and renders the answer in
-- a docked dialog. The auxiliary request inherits the live session so it
-- can reuse the same cached prefix as the main conversation.

local MAX_TOOL_CALL_RESTARTS = 2
local TOOL_DENIED_MESSAGE = "Tool use is not allowed for /btw. Respond with text only."

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
    local buf = smelt.buf.new({ mode = "markdown", readonly = true })
    local done = false

    local function tick()
      if done then return end
      buf:source(smelt.spinner.glyph() .. " working")
      smelt.timer.set(smelt.spinner.period_ms(), tick)
    end
    tick()

    local base_messages = build_base_messages(question)
    local restarts = 0

    local function send(messages, retrying_after_tool_denial)
      smelt.engine.ask_inherited({
        messages = messages,
        model = smelt.model.preferred("btw"),
        on_response = function(response, err)
          if err then
            done = true
            buf:source("error (" .. err.kind .. "): " .. err.message)
            return
          end
          if response and response.tool_calls and #response.tool_calls > 0 then
            if not retrying_after_tool_denial then
              send(append_tool_denials(messages, response), true)
              return
            end
            if restarts < MAX_TOOL_CALL_RESTARTS then
              restarts = restarts + 1
              send(base_messages, false)
              return
            end
            done = true
            buf:source("error (invalid_response): model kept requesting tools")
            return
          end
          done = true
          buf:source((response and response.content) or "")
        end,
      })
    end

    send(base_messages, false)

    local leaf = smelt.dialog.content({ buf = buf, interactive = true })

    smelt.dialog.open({
      title      = question,
      min_height = "30%",
      max_height = "70%",
      panels     = { { leaf = leaf, height = "fill" } },
      keymaps = {
        { key = "q", on_press = function(ctx) ctx.close() end },
      },
    })
  end)
end, { desc = "ask a side question", args = { "<question>" } })
