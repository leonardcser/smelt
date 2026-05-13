-- Built-in /help command. Centered info viewer of all keybindings.

local function build_lines(sections)
  local max_label = 0
  for _, section in ipairs(sections) do
    for _, entry in ipairs(section.entries) do
      if #entry.label > max_label then
        max_label = #entry.label
      end
    end
  end
  local label_col = max_label + 4

  local lines = {}
  for si, section in ipairs(sections) do
    for _, entry in ipairs(section.entries) do
      local padding = string.rep(" ", math.max(0, label_col - #entry.label))
      table.insert(lines, entry.label .. padding .. entry.detail)
    end
    if si < #sections then
      table.insert(lines, "")
    end
  end
  return lines
end

smelt.cmd.register("help", function()
  smelt.spawn(function()
    local sections = smelt.keymap.help()
    local lines = build_lines(sections)

    local buf = smelt.buf.create()
    smelt.buf.set_lines(buf, lines)
    local leaf = smelt.win.open(buf, { region = "dialog_overlay", focusable = true, vim_enabled = true })

    smelt.ui.overlay.open({
      title     = "help",
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
end, { desc = "show keybindings" })

-- `?` opens /help unless the prompt has content or vim is in Normal/Visual mode.
-- Returns false to let the literal `?` fall through to the buffer.
smelt.keymap.set("", "?", function()
  if smelt.win.focus() == "prompt" then
    local txt = smelt.prompt.text()
    local vim_mode = smelt.vim.mode()
    if vim_mode == "normal" or vim_mode == "visual" or vim_mode == "visual_line" then
      return false
    end
    if txt and txt ~= "" then
      return false
    end
  end
  smelt.cmd.run("help")
end)
