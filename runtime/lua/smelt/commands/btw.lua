-- Built-in /btw command. Asks a side question; streams the answer into a markdown dialog.

local SYSTEM = "You are a helpful assistant. The user is asking a quick side question "
  .. "while working on something else. Answer concisely and directly. "
  .. "You have the conversation history for context."

smelt.cmd.register("btw", function(args)
  local question = args or ""
  if question == "" then
    smelt.ui.notify_error("usage: /btw <question>")
    return
  end

  smelt.spawn(function()
    local buf = smelt.buf.create({ mode = "markdown" })
    local done = false

    local function tick()
      if done then return end
      smelt.buf.set_source(buf, smelt.ui.spinner.glyph() .. " working")
      smelt.defer(smelt.ui.spinner.period_ms(), tick)
    end
    tick()

    local history = smelt.session.messages()
    local messages = {}
    for _, msg in ipairs(history) do
      table.insert(messages, { role = msg.role, content = msg.content or "" })
    end
    table.insert(messages, { role = "user", content = question })

    smelt.engine.ask({
      system = SYSTEM,
      messages = messages,
      task = "btw",
      on_response = function(content)
        done = true
        smelt.buf.set_source(buf, content)
      end,
    })

    smelt.ui.dialog.open({
      title = question,
      panels = {
        { kind = "content", buf = buf, height = "fill" },
      },
    })
  end)
end, { desc = "ask a side question" })
