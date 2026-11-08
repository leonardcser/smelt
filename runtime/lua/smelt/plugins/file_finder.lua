-- /files and /search: find workspace or Git-tracked files by name, or search
-- their contents, with a source preview beside the results.
local active = false
local FILE_LIMIT = 500

-- Shows one file in the preview pane. Previews are native documents that
-- build only visible rows, so every selection change renders immediately.
local function previewer(preview, preview_buf)
  local key
  local function notice(text)
    preview:document(nil)
    preview_buf:lines({ "  " .. text })
  end
  return function(path, opts)
    opts = opts or {}
    local next_key = path and table.concat({ path, opts.line or 0, opts.query or "" }, "\0") or false
    if next_key == key then return end
    key = next_key
    if not path then return notice("(no file selected)") end
    if not smelt.fs.is_file(path) then return notice("(file unavailable)") end
    local doc, err = smelt.document.file(path, { query = opts.query, line = opts.line })
    if not doc then
      return notice(err == "binary" and "(binary file)" or ("Preview unavailable: " .. (err or "cannot read file")))
    end
    preview:document(doc)
    local top = 0
    if opts.line then
      -- Keep some context above the focused line.
      local height = (preview:rect() or {}).height or 0
      top = math.max(0, opts.line - 1 - math.floor(height / 3))
    end
    preview:scroll(top)
  end
end

-- The overlay both pickers share: results above a query input, and a preview
-- pane on terminals wide enough for one. `spec` supplies the behaviour:
-- `query(text)`, `move(delta)`, `selected()`, `submit()`, `close()` and an
-- optional `layout()` run before the list paints at a new height, plus the
-- `list_width` beside the preview.
local function shell(spec)
  local layout = smelt.ui.layout
  local ui = { closed = false }

  local input_buf = smelt.buf.new()
  input_buf:lines({ "" })
  local input = smelt.win.new(input_buf, {
    surface = "editable_text", kind = "input", placeholder = spec.placeholder,
    pad_left = 1, pad_right = 1, wrap = false, scrollbar = false,
  })
  local count_buf = smelt.buf.new({ readonly = true })
  count_buf:lines({ "" })
  local count_win = smelt.win.new(count_buf, {
    surface = "inert", wrap = false, scrollbar = false,
    pad_right = 1, style = "Comment", focus_on_click = input,
  })
  local count_size = layout.measure(6, 1)
  function ui.count(label)
    if count_buf:line(1) == label then return end
    count_buf:lines({ label })
    count_size:set(#label + 1, 1)
  end

  ui.list_buf = smelt.buf.new()
  ui.list_win = smelt.win.new(ui.list_buf, {
    surface = "list_inert", kind = "list", initial_cursor = 0,
    pad_left = 1, pad_right = 1, scrollbar = false,
  })
  local preview_buf = smelt.buf.new({ readonly = true })
  local preview = smelt.win.new(preview_buf, {
    name = spec.name .. ".preview", surface = "selectable_text", wrap = false, scrollbar = true, gutter = "line_numbers",
  })
  local show = previewer(preview, preview_buf)
  -- The preview needs room beside the list; narrower terminals show the list alone.
  local function wants_preview() return (smelt.ui.size().width or 80) >= 100 end
  local show_preview = wants_preview()
  local shown = {}
  function ui.preview(path, opts)
    shown = { path, opts }
    if show_preview then show(path, opts) end
  end

  local overlay
  function ui.close()
    if ui.closed then return end
    ui.closed, active = true, false
    spec.close()
    if overlay then overlay:close() end
  end

  local keymaps = {
    { key = "enter", on_press = spec.submit },
    { key = "esc", on_press = ui.close },
    { key = "ctrl-c", on_press = ui.close },
  }
  for _, binding in ipairs({
    { "up", -1 }, { "down", 1 }, { "ctrl-k", -1 }, { "ctrl-j", 1 },
    { "ctrl-p", -1 }, { "ctrl-n", 1 }, { "pgup", -10 }, { "pgdn", 10 },
  }) do
    local function handler() spec.move(binding[2]) end
    input:key(binding[1], handler)
    keymaps[#keymaps + 1] = { key = binding[1], on_press = handler }
  end
  input:on("text_changed", function(event) spec.query((event and event.text) or "") end)
  input:on("submit", spec.submit)
  ui.list_win:on("submit", spec.submit)
  ui.list_win:on("selection_changed", spec.selected)
  for _, win in ipairs({ input, ui.list_win, preview }) do win:on("dismiss", ui.close) end

  -- Dragging the divider resizes the results and the preview; the handle
  -- outlives remounts so the chosen width survives terminal resizes.
  local split = layout.split("horizontal", { key = "file_finder", size = spec.list_width or "40%", min_first = 30, min_second = 30 })
  local function mount()
    -- Results and input share one frame: the input's top edge is the
    -- separator. With a preview, the split divider is the shared right edge.
    local list_edge = { top = "Comment", left = "Comment" }
    local input_edge = { top = "Comment", left = "Comment", bottom = "Comment" }
    if not show_preview then
      list_edge.right = "Comment"
      input_edge.right = "Comment"
    end
    local left = layout.vbox({
      { layout.leaf(ui.list_win, { border = list_edge }), height = "fill" },
      { layout.hbox({
        { layout.leaf(input), width = "fill" },
        { layout.leaf(count_win, { measure = count_size }), width = "fit" },
      }, { border = input_edge, gap = 0 }), height = 3 },
    }, { gap = 0 })
    local root = left
    if show_preview then
      root = split:layout(left, layout.leaf(preview, {
        border = { top = "Comment", bottom = "Comment", right = "Comment" },
        title = smelt.dialog.title(" preview "),
      }))
    end
    overlay = smelt.overlay.new({
      name = spec.name, anchor = "center", border = "none", modal = true,
      width = "85%", height = "75%", layout = root, keymaps = keymaps,
    })
  end
  mount()
  input:focus()
  if spec.layout then
    ui.list_win:set_renderer(function() if not ui.closed then spec.layout() end end)
  end
  ui.list_win:on("resized", function()
    local wide = wants_preview()
    if ui.closed or wide == show_preview then return end
    show_preview = wide
    mount()
    input:focus()
    if wide then show(shown[1], shown[2]) end
  end)
  return ui
end

local function open_files(tracked)
  local cwd = smelt.session.cwd()
  local result_root = cwd
  local source = tracked and "git" or "workspace"
  local query, displayed_query = ""
  local ui, list, poll

  local function show_selected()
    if not list then return end
    local item = list:selected()
    ui.preview(item and smelt.path.join(result_root, item.path))
  end

  local function submit()
    local item = list:selected()
    if not item then return end
    ui.close()
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
      ui.list_win:scroll("tail")
    else
      list:set_items_preserve(items, function(item) return item.path end)
    end
    show_selected()
  end

  local function refilter(reset)
    local result = smelt.files.search(query, { cwd = cwd, source = source, limit = FILE_LIMIT, include_dirs = false })
    if result.status == "error" then
      smelt.notify.error(result.message or "cannot search files")
      ui.close()
      return
    end
    if __smelt_internal.provider.should_keep_stale_rows(result, result.items, list:visible()) then return end
    result_root = result.root or cwd
    ui.count(string.format("%d / %d", result.total_matched, result.total_files))
    show(result.items, reset or displayed_query ~= query)
    displayed_query = query
  end

  ui = shell({
    name = "smelt.files",
    placeholder = "find files…",
    query = function(text) query = text; refilter(true) end,
    move = function(delta) list:move_cursor(delta); show_selected() end,
    selected = show_selected,
    submit = submit,
    close = function() if poll then poll:remove() end end,
  })
  list = smelt.list.new({
    leaf = ui.list_win, buf = ui.list_buf, items = {},
    render = require("smelt.widgets.file_row").render,
    anchor = "bottom",
    empty_text = "  (no matching files)",
  })
  poll = smelt.timer.every(100, function() if not ui.closed then refilter(false) end end)
  refilter(true)
end

local function open_search(tracked)
  local cwd = smelt.session.cwd()
  local source = tracked and "git" or "workspace"
  local query = ""
  -- The search being typed, and the one whose rows the list shows. They
  -- differ until the new search finds a row or finishes, so typing never
  -- flashes an empty list. Rows are reversed: match 1 sits above the input.
  local search, shown, version
  -- 1-based index of the selected match in `shown`, and its match count.
  local selected, rows = 1, 0
  local ui, poll

  local function height() return (ui.list_win:rect() or {}).height or 0 end

  local function selected_item()
    if not shown then return nil end
    return shown:item(ui.list_win:cursor() or 0)
  end

  local function show_selected()
    local item = selected_item()
    if item then selected = item.index end
    ui.preview(item and smelt.path.join(shown:root(), item.path), item and { query = query, line = item.line })
  end

  -- The message the list shows instead of matches, if any.
  local notice_text

  local function draw_notice()
    local text = notice_text
    local lines = {}
    for i = 1, math.max(0, height() - 1) do lines[i] = "" end
    lines[#lines + 1] = text
    ui.list_buf:lines(lines)
    ui.list_buf:mark(smelt.ns("smelt.search"), #lines, 0, { end_col = #text, style = { dim = true } })
    ui.list_win:scroll("tail")
    ui.list_win:cursor(#lines - 1)
  end

  local function notice(text)
    if shown then shown:cancel() end
    shown, notice_text = nil, text
    ui.list_win:document(nil)
    draw_notice()
    show_selected()
  end

  local function start()
    if search and search ~= shown then search:cancel() end
    search, version = nil, nil
    if query == "" then
      ui.count("")
      return notice("  (type to search)")
    end
    local err
    search, err = smelt.files.grep(query, {
      cwd = cwd, source = source, reverse = true,
      icons = smelt.settings.file_icons
        and { colors = smelt.settings.file_icon_colors, light = smelt.theme.is_light() } or nil,
    })
    if not search then
      ui.count("")
      return notice("  " .. (err or "cannot search"))
    end
    search:fill(height())
  end

  -- Rows of a reversed document grow above; keep the selection and the rows
  -- on screen still.
  local function shift(added)
    if added ~= 0 then ui.list_win:scroll(math.max(0, ui.list_win:scroll().top + added)) end
    ui.list_win:cursor(shown:row(selected) or 0)
  end

  local function update()
    if not search then return end
    local progress = search:progress()
    if progress.version == version then return end
    version = progress.version
    local label = string.format("%d %s", progress.total, progress.total == 1 and "match" or "matches")
    if not progress.done and progress.files_total > 0 then
      label = string.format("%s · %d%%", label, math.floor(100 * progress.files_searched / progress.files_total))
    end
    ui.count(label)
    if shown == search then
      rows = progress.rows
      shift(search:sync())
    elseif progress.rows > 0 then
      if shown then shown:cancel() end
      search:sync()
      shown, selected, rows, notice_text = search, 1, progress.rows, nil
      ui.list_win:document(search:document())
      ui.list_win:scroll("tail")
      ui.list_win:cursor(search:row(1))
      show_selected()
    elseif progress.done then
      notice("  (no matches)")
    end
    -- A search started while the file index was still building covers only
    -- part of the workspace; run it again once that one is done.
    if progress.done and progress.indexing then start() end
  end

  local function move(delta)
    if not shown then return end
    local index = math.max(1, math.min(rows, selected - delta))
    ui.list_win:cursor(shown:row(index))
    show_selected()
  end

  local function submit()
    local item = selected_item()
    if not item then return end
    local path = smelt.path.join(shown:root(), item.path)
    ui.close()
    local ok, err = smelt.terminal.open_editor(path, { line = item.line })
    if not ok then smelt.notify.error(err or "could not open editor") end
  end

  ui = shell({
    name = "smelt.search",
    placeholder = "search files…",
    list_width = "55%",
    query = function(text) query = text; start(); update() end,
    move = move,
    selected = show_selected,
    submit = submit,
    layout = function()
      if search and search ~= shown then search:fill(height()) end
      if shown then
        shift(shown:fill(height()))
      elseif notice_text then
        draw_notice()
      end
    end,
    close = function()
      if poll then poll:remove() end
      if search then search:cancel() end
      if shown then shown:cancel() end
    end,
  })
  poll = smelt.timer.every(16, function() if not ui.closed then update() end end)
  start()
end

local function command(name, open)
  return function(arg)
    local source = (arg or ""):match("^%s*(.-)%s*$")
    if active then return end
    if source == "" or source == "all" then
      active = true
      open(false)
    elseif source == "git" then
      active = true
      open(true)
    else
      smelt.notify.error("usage: /" .. name .. " [all|git]")
    end
  end
end

smelt.cmd.register("files", command("files", open_files),
  { desc = "find workspace or Git-tracked file", args = { "[all|git]" }, busy = "run" })
smelt.cmd.register("search", command("search", open_search),
  { desc = "search workspace or Git-tracked file contents", args = { "[all|git]" }, busy = "run" })
