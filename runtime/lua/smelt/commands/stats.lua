-- Built-in /stats and /cost commands. Docked-bottom dialogs; q/Esc dismiss.

local function open_text_dialog(title, text)
  smelt.spawn(function()
    local buf = smelt.buf.new({ readonly = true })
    local lines = {}
    for line in (text or ""):gmatch("([^\n]*)\n?") do table.insert(lines, line) end
    if #lines == 0 then lines = { "" } end
    buf:lines(lines)
    local leaf = smelt.ui.dialog.content({ buf = buf, interactive = true })

    smelt.ui.dialog.open({
      title      = title,
      max_height = "50%",
      panels     = { { leaf = leaf } },
      keymaps    = {
        { key = "q", on_press = function(ctx) ctx.close() end },
        { key = "?", on_press = function(ctx) ctx.close() end },
      },
    })
  end)
end

smelt.cmd.register("stats", function()
  open_text_dialog("stats", smelt.metrics.stats_text())
end, { desc = "show token usage statistics" })

smelt.cmd.register("cost", function()
  open_text_dialog("cost", smelt.metrics.session_cost_text())
end, { desc = "show session cost" })
