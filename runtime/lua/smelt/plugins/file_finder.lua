-- /files (all or git): fuzzy workspace or Git-tracked file search with source preview.
local active = false
local RESULT_LIMIT = 500
local PREVIEW_BYTES = 64 * 1024

local function open(tracked)
  if active then return end
  active = true
  local cwd = smelt.session.cwd()
  local result_root = cwd
  local source = tracked and "git" or "workspace"
  local layout = smelt.ui.layout
  local query = ""
  local displayed_query
  local closed, overlay, poll, preview_timer = false
  local preview_path

  local input_buf = smelt.buf.new()
  input_buf:lines({ "" })
  local input = smelt.win.new(input_buf, {
    surface = "editable_text", kind = "input", placeholder = "find files…",
    pad_left = 1, pad_right = 1, wrap = false, scrollbar = false,
  })
  local count_buf = smelt.buf.new({ readonly = true })
  count_buf:lines({ "0 / 0" })
  local count_win = smelt.win.new(count_buf, {
    surface = "inert", wrap = false, scrollbar = false,
    pad_right = 1, style = "Comment", focus_on_click = input,
  })
  local count_size = layout.measure(6, 1)
  local function update_count(matched, total)
    local label = string.format("%d / %d", matched, total)
    if count_buf:line(1) == label then return end
    count_buf:lines({ label })
    count_size:set(#label + 1, 1)
  end

  local list_buf = smelt.buf.new()
  local list_win = smelt.win.new(list_buf, {
    surface = "list_inert", kind = "list", initial_cursor = 0,
    pad_left = 1, pad_right = 1, scrollbar = false,
  })
  local preview_buf, preview
  if (smelt.ui.size().width or 80) >= 100 then
    preview_buf = smelt.buf.new({ readonly = true })
    preview_buf:lines({ "  (no file selected)" })
    preview = smelt.win.new(preview_buf, {
      surface = "selectable_text", wrap = false, scrollbar = true, gutter = "line_numbers",
    })
  end
  local list = smelt.list.new({
    leaf = list_win, buf = list_buf, items = {},
    render = require("smelt.widgets.file_row").render,
    anchor = "bottom",
    empty_text = "  (no matching files)",
  })

  local function render_preview()
    local item = list:selected()
    local path = item and smelt.path.join(result_root, item.path)
    if preview_path == path then return end
    preview_path = path
    preview:scroll(0)
    if not path then
      preview_buf:lines({ "  (no file selected)" })
      return
    end
    if not smelt.fs.is_file(path) then
      preview_buf:lines({ "  (file unavailable)" })
      return
    end
    -- A size-limited read also bounds syntax-highlighting work on the UI thread.
    local result, err = smelt.fs.read_limited(path, PREVIEW_BYTES)
    if not result then
      preview_buf:lines({ "  Preview unavailable: " .. (err or "cannot read file") })
    elseif result.content:find("\0", 1, true) then
      preview_buf:lines({ "  (binary file)" })
    else
      preview_buf:lines({})
      smelt.render.syntax(preview_buf, { content = result.content, path = path, line_numbers = true })
      if result.truncated then smelt.render.text(preview_buf, "\n[preview truncated]") end
    end
  end

  -- The first preview renders immediately; later cursor movement is debounced.
  local function schedule_preview()
    if not preview then return end
    if preview_timer then preview_timer:remove(); preview_timer = nil end
    if not preview_path then return render_preview() end
    preview_timer = smelt.timer.set(40, function()
      preview_timer = nil
      if not closed then render_preview() end
    end)
  end

  local function close()
    if closed then return end
    closed, active = true, false
    if poll then poll:remove() end
    if preview_timer then preview_timer:remove() end
    if overlay then overlay:close() end
  end

  local function submit()
    local item = list:selected()
    if not item then return end
    close()
    if not tracked then smelt.files.accept(item, { cwd = result_root }) end
    local ok, err = smelt.terminal.open_editor(smelt.path.join(result_root, item.path))
    if not ok then smelt.notify.error(err or "could not open editor") end
  end

  local function show(ranked, reset)
    local items = {}
    for i = #ranked, 1, -1 do items[#items + 1] = ranked[i] end
    local visible = list:visible()
    local same = #items == #visible
    if same then
      for i, item in ipairs(items) do
        if visible[i].path ~= item.path then same = false; break end
      end
    end
    if same and not reset then return end
    if reset or list:selected() == nil then
      list:set_items(items)
      list:set_cursor(#items - 1)
      list_win:scroll("tail")
    else
      list:set_items_preserve(items, function(item) return item.path end)
    end
    schedule_preview()
  end

  local function refilter(reset)
    local result = smelt.files.search(query, { cwd = cwd, source = source, limit = RESULT_LIMIT, include_dirs = false })
    if result.status == "error" then
      smelt.notify.error(result.message or "cannot search files")
      close()
      return
    end
    if __smelt_internal.provider.should_keep_stale_rows(result, result.items, list:visible()) then return end
    result_root = result.root or cwd
    update_count(result.total_matched, result.total_files)
    show(result.items, reset or displayed_query ~= query)
    displayed_query = query
  end

  local function nav(delta)
    return function() list:move_cursor(delta); schedule_preview() end
  end
  local bindings = {
    { "up", -1 }, { "down", 1 }, { "ctrl-k", -1 }, { "ctrl-j", 1 },
    { "ctrl-p", -1 }, { "ctrl-n", 1 }, { "pgup", -10 }, { "pgdn", 10 },
  }
  local keymaps = {
    { key = "enter", on_press = submit },
    { key = "esc", on_press = close },
    { key = "ctrl-c", on_press = close },
  }
  for _, binding in ipairs(bindings) do
    local handler = nav(binding[2])
    input:key(binding[1], handler)
    keymaps[#keymaps + 1] = { key = binding[1], on_press = handler }
  end
  input:on("text_changed", function(event)
    query = (event and event.text) or ""
    refilter(true)
  end)
  input:on("submit", submit)
  list_win:on("submit", submit)
  list_win:on("selection_changed", schedule_preview)
  for _, win in ipairs({ input, list_win, preview }) do win:on("dismiss", close) end

  local left = layout.vbox({
    { layout.leaf(list_win, {
      border = { all = "Comment" },
    }), height = "fill" },
    { layout.hbox({
      { layout.leaf(input), width = "fill" },
      { layout.leaf(count_win, { measure = count_size }), width = "fit" },
    }, { border = { all = "Comment" }, gap = 0 }), height = 3 },
  }, { gap = 0 })
  local root = left
  if preview then
    root = layout.hbox({
      { left, width = "40%" },
      { layout.leaf(preview, {
        border = { all = "Comment" }, title = smelt.dialog.title(" preview "),
      }), width = "fill" },
    }, { gap = 0, padding = 0 })
  end
  overlay = smelt.overlay.new({
    anchor = "center", border = "none", modal = true,
    width = "85%", height = "75%", layout = root, keymaps = keymaps,
  })
  input:focus()

  poll = smelt.timer.every(100, function() if not closed then refilter(false) end end)
  refilter(true)
end

smelt.cmd.register("files", function(arg)
  local source = (arg or ""):match("^%s*(.-)%s*$")
  if source == "" or source == "all" then
    open(false)
  elseif source == "git" then
    open(true)
  else
    smelt.notify.error("usage: /files [all|git]")
  end
end, { desc = "find workspace or Git-tracked file", args = { "[all|git]" }, busy = "run" })
