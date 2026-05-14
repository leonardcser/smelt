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

    local buf = smelt.buf.create({ readonly = true })
    smelt.buf.set_lines(buf, lines)
    -- `selectable = true` so mouse click-and-drag highlights text the same way
    -- it does in the transcript; vim_enabled gives keyboard nav + visual mode.
    local leaf = smelt.win.open(buf, {
      region      = "dialog_overlay",
      focusable   = true,
      selectable  = true,
      vim_enabled = true,
    })

    smelt.ui.overlay.open({
      title     = { { text = " help ", bold = true } },
      placement = "screen_center",
      border    = { all = "Comment" },
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

-- `?` opens /help unless it would land as text in an editable buffer the user
-- is actively typing into. That narrows the literal-`?` carve-out to: prompt
-- focus AND non-empty content AND vim is in insert mode (or vim is disabled,
-- so every keystroke is text). Vim normal/visual mode is always a "command"
-- context — `?` opens help even when the prompt has content. Returning false
-- passes the keystroke through to the buffer so it lands as a real `?`.
smelt.keymap.set("", "?", function()
  if smelt.win.focus() == "prompt" then
    local vim_mode = smelt.vim.mode()
    local typing = vim_mode == nil or vim_mode == "insert"
    if typing then
      local txt = smelt.prompt.text()
      if txt and txt ~= "" then
        return false
      end
    end
  end
  smelt.cmd.run("help")
end)
