-- A plan the agent keeps: a `todo` tool it calls with its steps, drawn in the transcript as a
-- checklist that ticks off as it works, with the step in progress lit up.
local L = smelt.layout
local MARKS = {
  done = { "✔", "TodoDone" },
  active = { "▶", "TodoActive" },
  todo = { "○", "TodoWaiting" },
}
smelt.theme.set("TodoDone", { fg = { 120, 210, 140 } })
smelt.theme.set("TodoActive", { fg = { 250, 190, 90 }, bold = true })
smelt.theme.set("TodoWaiting", { fg = { 130, 130, 148 } })

smelt.tools.register({
  name = "todo",
  description = "Show the user your plan as a checklist. Call it again with new statuses as you go.",
  parameters = {
    type = "object",
    properties = {
      items = {
        type = "array",
        items = {
          type = "object",
          properties = {
            text = { type = "string" },
            status = { type = "string", enum = { "todo", "active", "done" } },
          },
          required = { "text", "status" },
        },
      },
    },
    required = { "items" },
  },
  summary = function(args)
    local done = 0
    for _, item in ipairs(args.items or {}) do
      if item.status == "done" then done = done + 1 end
    end
    return string.format("%d of %d done", done, #(args.items or {}))
  end,
  execute = function() return "ok" end,
})

smelt.transcript.register_tool("todo", {
  body = function(tool)
    local rows = {}
    for _, item in ipairs(tool.args.items or {}) do
      local mark = MARKS[item.status] or MARKS.todo
      rows[#rows + 1] = L.line({
        { text = "  " .. mark[1] .. " ", hl = mark[2], bold = true, selectable = false },
        {
          text = item.text,
          hl = item.status == "todo" and "TodoWaiting" or nil,
          dim = item.status == "done",
          bold = item.status == "active",
        },
      })
    end
    return L.vbox(rows)
  end,
})
