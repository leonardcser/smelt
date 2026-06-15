-- Built-in /help command. Responsive info viewer of all keybindings.

local NS_HELP = smelt.ns("smelt.help")

local GAP = "    "
local MIN_WIDTH = 58
local MAX_WIDTH = 110

local function lower_title(title)
  return tostring(title or "keybindings"):lower()
end

local function clamp_width(term_w)
  local term = math.max(math.floor(term_w or 80), 24)
  local w = math.floor(term * 0.85)
  w = math.max(math.min(MIN_WIDTH, term - 2), w)
  w = math.min(MAX_WIDTH, w, term - 2)
  return math.max(24, w)
end

local function column_count(width)
  if width >= 96 then return 3 end
  if width >= 68 then return 2 end
  return 1
end

local function entry_label_width(entries, col_w)
  local max_label = 0
  for _, entry in ipairs(entries) do
    max_label = math.max(max_label, smelt.text.width(entry.label or ""))
  end
  return math.min(max_label + 2, math.max(8, math.floor(col_w * 0.42)))
end

local function add_mark(marks, row, col, end_col, opts)
  if end_col > col then
    marks[#marks + 1] = { row = row, col = col, end_col = end_col, opts = opts }
  end
end

local function render_entry(entry, col_w, label_w)
  local label = smelt.text.fit(entry.label or "", label_w, { suffix = "…" })
  local detail_w = math.max(col_w - label_w, 0)
  local detail = smelt.text.fit(entry.detail or "", detail_w, { suffix = "…" })
  return label .. detail, #label, #label + #detail
end

local function build_layout(sections, width)
  local lines = {}
  local marks = {}
  for _, section in ipairs(sections) do
    local title = lower_title(section.title)
    lines[#lines + 1] = title
    add_mark(marks, #lines, 0, #title, { fg = "Comment", bold = true })

    local entries = section.entries or {}
    local cols = math.min(column_count(width), math.max(#entries, 1))
    local gap_w = smelt.text.width(GAP)
    local col_w = math.floor((width - gap_w * (cols - 1)) / cols)
    local label_w = entry_label_width(entries, col_w)
    local rows = math.ceil(#entries / cols)

    for row = 1, rows do
      local line = ""
      local row_marks = {}
      for col = 1, cols do
        local idx = row + (col - 1) * rows
        local cell = ""
        local label_end = 0
        local detail_end = 0
        if entries[idx] then
          cell, label_end, detail_end = render_entry(entries[idx], col_w, label_w)
        else
          cell = string.rep(" ", col_w)
        end
        local start = #line
        line = line .. cell
        if entries[idx] then
          row_marks[#row_marks + 1] = { start = start, label_end = start + label_end, detail_end = start + detail_end }
        end
        if col < cols then
          line = line .. GAP
        end
      end
      lines[#lines + 1] = line:gsub("%s+$", "")
      for _, mark in ipairs(row_marks) do
        add_mark(marks, #lines, mark.start, mark.label_end, { fg = "Comment" })
        add_mark(marks, #lines, mark.label_end, mark.detail_end, { dim = true })
      end
    end
  end
  return lines, marks
end

smelt.cmd.register("help", function()
  smelt.spawn(function()
    local sections = smelt.keymap.help()
    local size = smelt.ui.size()
    local width = clamp_width(size.width)
    local lines, marks = build_layout(sections, width)
    local measure = smelt.ui.layout.measure(width, #lines)

    local buf = smelt.buf.new({ readonly = true })
    local leaf = smelt.win.new(buf, {
      region      = "dialog_overlay",
      surface     = "readonly_text",
      wrap        = false,
      hide_cursor = true,
      vim_enabled = smelt.settings.vim and true or false,
    })

    local function paint()
      local content_width = leaf:content_width() or width
      lines, marks = build_layout(sections, content_width)
      measure:set(content_width, #lines)
      buf:lines(lines):clear_ns(NS_HELP)
      for _, mark in ipairs(marks) do
        local opts = mark.opts or {}
        opts.end_col = mark.end_col
        buf:mark(NS_HELP, mark.row, mark.col, opts)
      end
    end

    paint()
    leaf:on("resized", paint)

    smelt.overlay.new({
      title     = { { text = " help ", bold = true } },
      anchor    = "center",
      border    = { all = "Comment" },
      modal     = true,
      width      = width + 2,
      max_width  = "100%",
      max_height = "100%",
      layout    = smelt.ui.layout.leaf(leaf, { measure = measure }),
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
