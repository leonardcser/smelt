-- Built-in /stats and /cost commands. Centered scrollable text viewers; q/Esc dismiss.

local function open_text_modal(title, text)
  smelt.spawn(function()
    local buf = smelt.buf.create()
    local lines = {}
    for line in (text or ""):gmatch("([^\n]*)\n?") do table.insert(lines, line) end
    if #lines == 0 then lines = { "" } end
    smelt.buf.set_lines(buf, lines)
    local leaf = smelt.win.open(buf, { region = "dialog_overlay", focusable = true, vim_enabled = true })

    smelt.ui.overlay.open({
      title     = title,
      placement = "screen_center",
      border    = "single",
      modal     = true,
      items     = { { win = leaf, height = "fill" } },
    })

    local task_id = smelt.task.alloc()
    local function close() smelt.win.close(leaf); smelt.task.resume(task_id, nil) end
    smelt.win.set_keymap(leaf, "q", close)
    smelt.win.set_keymap(leaf, "?", close)
    smelt.win.on_event(leaf, "dismiss", close)
    smelt.task.wait(task_id)
  end)
end

smelt.cmd.register("stats", function()
  open_text_modal("stats", smelt.metrics.stats_text())
end, { desc = "show token usage statistics" })

smelt.cmd.register("cost", function()
  open_text_modal("cost", smelt.metrics.session_cost_text())
end, { desc = "show session cost" })
