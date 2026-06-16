-- Built-in /help command. Responsive info viewer of all keybindings.

local NS_HELP = smelt.ns("smelt.help")

local GAP = "    "
local SECTION_INDENT = "  "
local MIN_WIDTH = 24

local function lower_title(title)
  return tostring(title or "keybindings"):lower()
end

local function overlay_content_width(term_w)
  local term = math.max(math.floor(term_w or 80), MIN_WIDTH)
  return math.max(MIN_WIDTH, math.floor(term * 0.85) - 2)
end

local function column_count(width)
  if width >= 88 then return 2 end
  return 1
end

local function entry_label_width(entries, col_w)
  local max_label = 0
  for _, entry in ipairs(entries) do
    max_label = math.max(max_label, smelt.text.width(entry.label or ""))
  end
  return math.min(max_label + 2, math.max(12, math.floor(col_w * 0.55)))
end

local function add_mark(marks, row, col, end_col, opts)
  if end_col > col then
    marks[#marks + 1] = { row = row, col = col, end_col = end_col, opts = opts }
  end
end

local function spaces(width)
  return string.rep(" ", math.max(width, 0))
end

local function pad_to_width(s, width)
  return s .. spaces(width - smelt.text.width(s))
end

local function split_word(word, width)
  local lines = {}
  local line = ""
  local line_w = 0
  for _, codepoint in utf8.codes(word) do
    local ch = utf8.char(codepoint)
    local ch_w = smelt.text.width(ch)
    if line ~= "" and line_w + ch_w > width then
      lines[#lines + 1] = line
      line = ""
      line_w = 0
    end
    line = line .. ch
    line_w = line_w + ch_w
  end
  if line ~= "" then lines[#lines + 1] = line end
  return lines
end

local function wrap_text(s, width)
  if width <= 0 then return { "" } end

  local lines = {}
  local line = ""
  local line_w = 0
  for word in tostring(s or ""):gmatch("%S+") do
    local word_w = smelt.text.width(word)
    if word_w > width then
      if line ~= "" then
        lines[#lines + 1] = line
        line = ""
        line_w = 0
      end
      for _, part in ipairs(split_word(word, width)) do
        lines[#lines + 1] = part
      end
    elseif line == "" then
      line = word
      line_w = word_w
    elseif line_w + 1 + word_w <= width then
      line = line .. " " .. word
      line_w = line_w + 1 + word_w
    else
      lines[#lines + 1] = line
      line = word
      line_w = word_w
    end
  end
  if line ~= "" then lines[#lines + 1] = line end
  if #lines == 0 then lines[1] = "" end
  return lines
end

local function render_entry(entry, col_w, label_cap)
  local label = tostring(entry.label or "")
  local detail = tostring(entry.detail or "")
  local label_w = math.min(smelt.text.width(label) + 2, label_cap)
  local detail_w = math.max(col_w - label_w, 0)
  local detail_lines = wrap_text(detail, detail_w)
  local lines = {}

  if smelt.text.width(label) <= label_w then
    lines[1] = pad_to_width(label, label_w) .. detail_lines[1]
    for i = 2, #detail_lines do
      lines[#lines + 1] = spaces(label_w) .. detail_lines[i]
    end
    return lines, #label
  end

  local label_lines = wrap_text(label, col_w)
  for _, line in ipairs(label_lines) do
    lines[#lines + 1] = line
  end
  for _, line in ipairs(detail_lines) do
    lines[#lines + 1] = spaces(label_w) .. line
  end
  return lines, #label_lines[1]
end

local function build_layout(sections, width)
  local lines = {}
  local marks = {}
  local entry_width = math.max(width - smelt.text.width(SECTION_INDENT), 1)
  for section_idx, section in ipairs(sections) do
    local title = lower_title(section.title)
    lines[#lines + 1] = title
    add_mark(marks, #lines, 0, #title, { fg = "SmeltHeading", bold = true })

    local entries = section.entries or {}
    local cols = math.min(column_count(entry_width), math.max(#entries, 1))
    local gap_w = smelt.text.width(GAP)
    local col_w = math.floor((entry_width - gap_w * (cols - 1)) / cols)
    local label_w = entry_label_width(entries, col_w)
    local rows = math.ceil(#entries / cols)

    for row = 1, rows do
      local cells = {}
      local row_height = 1
      for col = 1, cols do
        local idx = row + (col - 1) * rows
        if entries[idx] then
          local cell_lines, label_end = render_entry(entries[idx], col_w, label_w)
          cells[col] = { lines = cell_lines, label_end = label_end }
          row_height = math.max(row_height, #cell_lines)
        else
          cells[col] = { lines = { "" }, label_end = 0 }
        end
      end

      for visual_row = 1, row_height do
        local line = SECTION_INDENT
        local row_marks = {}
        for col = 1, cols do
          local cell = cells[col]
          local cell_line = cell.lines[visual_row] or ""
          local start = #line
          line = line .. pad_to_width(cell_line, col_w)
          if visual_row == 1 and cell.label_end > 0 then
            row_marks[#row_marks + 1] = {
              start = start,
              label_end = start + cell.label_end,
            }
          end
          if col < cols then
            line = line .. GAP
          end
        end

        lines[#lines + 1] = line:gsub("%s+$", "")
        for _, mark in ipairs(row_marks) do
          add_mark(marks, #lines, mark.start, mark.label_end, {
            fg = "SmeltAccent",
            bold = true,
          })
        end
      end
    end

    if section_idx < #sections then
      lines[#lines + 1] = ""
    end
  end
  return lines, marks
end

smelt.cmd.register("help", function()
  smelt.spawn(function()
    local sections = smelt.keymap.help()
    local size = smelt.ui.size()
    local width = overlay_content_width(size.width)

    local buf = smelt.buf.new({ readonly = true })
    local leaf = smelt.win.new(buf, {
      region      = "help_overlay",
      surface     = "readonly_text",
      wrap        = false,
      hide_cursor = true,
      vim_enabled = smelt.settings.vim and true or false,
    })

    local function paint()
      local content_width = leaf:content_width() or width
      local lines, marks = build_layout(sections, content_width)
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
      anchor = "center",
      border = "none",
      modal  = true,
      width  = "85%",
      height = "75%",
      layout = smelt.ui.layout.leaf(leaf, {
        border = { all = "Comment" },
        title = " help ",
      }),
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
