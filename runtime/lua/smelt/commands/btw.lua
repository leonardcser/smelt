-- Built-in /btw command. Asks a side question; streams the answer into a docked dialog.

local SYSTEM = "You are a helpful assistant. The user is asking a quick side question "
  .. "while working on something else. Answer concisely and directly. "
  .. "You have the conversation history for context."

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

    local history = smelt.session.messages()
    local messages = {}
    for _, msg in ipairs(history) do
      table.insert(messages, { role = msg.role, content = msg.content or "" })
    end
    table.insert(messages, { role = "user", content = question })

    smelt.engine.ask({
      system = SYSTEM,
      messages = messages,
      model = smelt.model.preferred("btw"),
      on_response = function(content, err)
        done = true
        if err then
          buf:source("error (" .. err.kind .. "): " .. err.message)
        else
          buf:source(content)
        end
      end,
    })

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
