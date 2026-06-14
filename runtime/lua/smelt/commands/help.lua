-- Built-in /help command. Centered info viewer of all keybindings.

local NS_DIM = smelt.ns("smelt.help.dim")

local function build_layout(sections)
  local max_label = 0
  for _, section in ipairs(sections) do
    for _, entry in ipairs(section.entries) do
      local w = smelt.text.width(entry.label)
      if w > max_label then max_label = w end
    end
  end
  local label_col = max_label + 4

  local lines = {}
  local detail_bytes = {}
  for si, section in ipairs(sections) do
    for _, entry in ipairs(section.entries) do
      local pad = label_col - smelt.text.width(entry.label)
      local line = entry.label .. string.rep(" ", pad) .. entry.detail
      table.insert(lines, line)
      table.insert(detail_bytes, #entry.label + pad)
    end
    if si < #sections then
      table.insert(lines, "")
      table.insert(detail_bytes, 0)
    end
  end
  return lines, detail_bytes
end

smelt.cmd.register("help", function()
  smelt.spawn(function()
    local sections = smelt.keymap.help()
    local lines, detail_bytes = build_layout(sections)

    local buf = smelt.buf.new({ readonly = true })
    buf:lines(lines)
    for i, line in ipairs(lines) do
      local col = detail_bytes[i]
      if col < #line then
        buf:mark(NS_DIM, i, col, { end_col = #line, dim = true })
      end
    end
    -- `wrap = false` so the leaf reports its actual longest-line width as its
    -- natural width - without this the overlay's `width = "fit"` would resolve
    -- to the full terminal.
    -- `surface = "readonly_text"` so mouse click-and-drag highlights text the same way
    -- it does in the transcript; vim_enabled tracks the user's global setting so
    -- non-vim users don't get dropped into normal mode here.
    local leaf = smelt.win.new(buf, {
      region      = "dialog_overlay",
      surface     = "readonly_text",
      wrap        = false,
      vim_enabled = smelt.settings.vim and true or false,
    })

    smelt.overlay.new({
      title  = { { text = " help ", bold = true } },
      anchor = "center",
      border = { all = "Comment" },
      modal  = true,
      layout = smelt.ui.layout.leaf(leaf),
    })

    local task_id = smelt.task.alloc()
    local function close() leaf:close(); smelt.task.resume(task_id, nil) end
    leaf:key("q", close)
    leaf:key("?", close)
    leaf:on("dismiss", close)
    smelt.task.wait(task_id)
  end)
end, { desc = "show keybindings" })

smelt.keymap.set("", "<F1>", function()
  smelt.cmd.run("help")
end)
