-- /diff: working changes, branch review and commit history. Layout, loading lifecycle,
-- sidebar synchronization and interaction policy live here; native documents
-- index patch bytes and materialize only the visible rows.

local M = {}
local active
-- The last snapshot is shown immediately on reopen while a fresh one loads,
-- then released if the viewer stays closed.
local RETAIN_MS = 5 * 60 * 1000
local retained

local function display(text)
  return (text or ""):gsub("%c", function(c) return string.format("\\x%02x", c:byte()) end)
end

function M.open(opts)
  opts = opts or {}
  if active then active.close() end
  local cwd = smelt.session.cwd()

  local closed, loading, changing, overlay = false, nil, nil, nil
  local diff, selected, busy
  local stale = false
  local file_count = 0
  local focused = "preview"
  local tree, sidebar_count, folder_press
  local layout = smelt.ui.layout
  local views = { "changes", "branch", "history" }
  -- Title cells the full tab row needs: " Changes  Branch  History ".
  local tabs_width = 26
  local compact_tabs = false
  -- Below this overlay width the two panes cannot both fit, so one shows at a time.
  local single_pane_width = 72
  local function fits_one_pane()
    return math.floor((smelt.ui.size().width or 80) * 0.94) < single_pane_width
  end
  local single_pane, shown_pane = fits_one_pane(), "preview"
  local view = opts.view or "changes"
  local browsing = view ~= "changes" and not opts.revision
  local base, revision = opts.base, opts.revision
  local base_id
  local commits, entries, head, more = {}, {}, nil, false
  local list_row, list_top = 0, 0
  local preview_job, list_error, comparison_label, branch_totals
  local load_history, select_commit, show_commits, switch_view
  local input_purpose = "revision"
  local query_open = false
  local saved = {}

  local function window(name, opts)
    local buf = smelt.buf.new()
    opts.name = "smelt.diff." .. name
    opts.wrap = false
    local win = smelt.win.new(buf, opts)
    return win, buf
  end

  local sidebar, sidebar_buf = window("files", {
    surface = "list", kind = "list", pad_left = 1, pad_right = 1,
    hide_cursor = true, scrollbar = true,
  })
  local preview, preview_buf = window("preview", {
    surface = "readonly_text", vim_enabled = true, scrollbar = true,
    pad_left = 1, pad_right = 1,
  })

  -- One-line placeholder for the preview: a dim message and a key hint that
  -- is dropped rather than cut off when the pane is narrow.
  local function notice(message, hint)
    preview:document(nil)
    local line = { { text = message, style = "Comment" } }
    local width = preview:content_width()
    if hint and (not width or smelt.text.width(message .. "  " .. hint) <= width) then
      line[2] = { text = "  " .. hint, style = { dim = true } }
    end
    preview_buf:styled({ line })
  end

  local input, input_buf = window("revision", {
    surface = "editable_text", kind = "input", vim_enabled = false, pad_left = 1,
    placeholder = "Commit ID or revision",
  })
  local info, info_buf = window("commit", { surface = "readonly_text", pad_left = 1, scrollbar = true })
  local split = layout.split("horizontal", { key = "diff", size = 42, min_first = 26, min_second = 32 })
  -- Before the first paint, estimate the sidebar from the overlay width so the
  -- first frame already uses the right tab labels.
  do
    local overlay_width = math.floor((smelt.terminal.size().width or 80) * 0.94)
    compact_tabs = math.min(42, overlay_width - 32) - 2 < tabs_width
  end
  local status
  local overlay_opts = { name = "smelt.diff", anchor = "center", width = "94%", height = "85%", modal = true, border = "none" }
  function status()
    local label = busy or (stale and "stale - r refresh")
    -- The sidebar's top border doubles as the view switcher; span ids make it clickable.
    local switcher = {}
    for _, tab in ipairs(views) do
      local name = tab:gsub("^%l", string.upper)
      -- Narrow panes keep the current tab's name and shorten the others to an initial.
      if compact_tabs and tab ~= view then name = name:sub(1, 1) end
      switcher[#switcher + 1] = { id = tab, text = " " .. name .. " ",
        style = tab == view and { fg = false, bg = "CursorLine", bold = true } or { dim = true } }
    end
    shown_pane = focused
    local files_leaf = layout.leaf(sidebar, {
      border = single_pane and { all = "Comment" } or { top = "Comment", bottom = "Comment", left = "Comment" },
      title = switcher,
    })
    local caption = label or comparison_label
    local title = caption and smelt.dialog.title(" " .. caption .. " ") or nil
    local pane_border = single_pane and { all = "Comment" }
      or { top = "Comment", bottom = "Comment", right = "Comment" }
    local detail = layout.leaf(preview, {
      border = pane_border, title = title,
    })
    if view ~= "changes" then
      detail = layout.frame(layout.vbox({
        { layout.leaf(info), height = 3 },
        { layout.leaf(preview, { border = { top = "Comment" }, title = title }), height = "fill" },
      }), {
        border = pane_border,
        title = smelt.dialog.title(browsing and " review " or " commit "),
      })
    end
    local rows = {
      { single_pane and (focused == "files" and files_leaf or detail) or split:layout(files_leaf, detail),
        height = "fill" },
      { layout.leaf(input, { border = { all = "Comment" },
        title = smelt.dialog.title(input_purpose == "base" and " comparison base " or " revision ") }),
        height = query_open and 3 or 0 },
    }
    -- Keep the metadata window alive when Changes hides it.
    if view == "changes" then rows[#rows + 1] = { layout.leaf(info), height = 0 } end
    overlay_opts.layout = layout.vbox(rows)
    overlay = smelt.overlay.new(overlay_opts)
  end

  local function build_tree()
    sidebar_count, folder_press = nil, nil
    local collapsed = tree and tree:collapsed() or {}
    tree = diff and diff:tree(collapsed) or nil
    if tree then
      sidebar:document(tree:document())
    else
      sidebar:document(nil)
      sidebar_buf:lines({ "unavailable" })
    end
  end

  local function selected_node()
    return tree and tree:node(sidebar:cursor() or 0)
  end
  local function reveal_file(index)
    if tree then sidebar:cursor(tree:file_row(index, true) or 0) end
  end
  local function toggle_directory()
    if not tree then return end
    local row = tree:toggle(sidebar:cursor() or 0)
    if row then sidebar:cursor(row) end
  end

  -- Cancel pending reads; index changes run to completion.
  local function stop_reads()
    if loading then loading:remove(); loading = nil end
    if preview_job then preview_job:remove(); preview_job = nil end
  end

  local function revision_info(value)
    info_buf:lines({ "Revision " .. display(value), "Read-only commit review", "Backspace returns to commits" })
  end

  local function cleanup()
    if closed then return end
    closed = true
    stop_reads()
    if changing then changing:remove(); changing = nil end
    active = nil
    -- Only working changes are retained; comparisons reload on demand.
    local changes = view == "changes" and diff or (saved.changes and saved.changes.diff)
    if changes and changes:file_count() > 0 then
      if retained then retained.timer:remove() end
      retained = { cwd = cwd, diff = changes }
      retained.timer = smelt.timer.set(RETAIN_MS, function() retained = nil end)
    end
    diff, tree, selected, folder_press = nil, nil, nil, nil
    saved, commits, entries = {}, {}, {}
    file_count = 0
  end

  local function close()
    cleanup()
    if overlay then overlay:close() end
  end
  preview:on("close", cleanup)
  sidebar:on("close", cleanup)

  local function jump_file(index, sidebar_row)
    sidebar_count, folder_press = nil, nil
    if not diff or file_count == 0 then return end
    index = math.max(1, math.min(file_count, index))
    if not selected then preview:document(diff:document()) end
    selected = index
    if sidebar_row then sidebar:cursor(sidebar_row) else reveal_file(index) end
    local row = diff:file_row(index)
    preview:scroll(row)
    preview:cursor(row)
  end

  local function sync_sidebar()
    if browsing or closed or not tree or not diff or not selected or folder_press or focused ~= "preview" then return end
    local scroll = preview:scroll()
    if not scroll then return end
    local index = diff:file_at(math.max(scroll.top, preview:cursor() or 0))
    if index and index ~= selected then
      selected = index
      -- Following the preview never expands deliberately collapsed folders.
      local row = tree:file_row(index)
      if row then sidebar:cursor(row) end
    end
  end

  local function focus_pane(name)
    sidebar_count, folder_press = nil, nil
    focused = name
    if single_pane and shown_pane ~= name then status() end
    if name == "preview" then preview:focus(); sync_sidebar() else sidebar:focus() end
  end
  sidebar:on("focus", function() focused = "files" end)
  sidebar:on("blur", function() sidebar_count, folder_press = nil, nil end)
  preview:on("focus", function() focused = "preview"; sync_sidebar() end)
  local function select_file()
    if browsing then select_commit(); return end
    local node = selected_node()
    if node and node.index and node.index ~= selected then jump_file(node.index) end
  end
  local function pointer_row(event)
    local rect, scroll = sidebar:rect(), sidebar:scroll()
    if not rect or not scroll or event.y >= rect.height or event.x < 1 or event.x >= rect.width - 1 then return end
    return scroll.top + event.y
  end
  sidebar:on("press", function(event)
    sidebar_count, folder_press = nil, nil
    if event.button ~= "left" then return end
    if event.title then
      switch_view(event.title)
      return
    end
    local row = pointer_row(event)
    if browsing then
      if row and entries[row + 1] then
        focus_pane("files")
        sidebar:cursor(row)
        select_commit()
      end
      return
    end
    local node = tree and row and tree:node(row)
    if not node then return end
    focus_pane("files")
    sidebar:cursor(row)
    if node.children then
      folder_press = { tree = tree, row = row }
    elseif node.index then
      jump_file(node.index)
    end
  end)
  sidebar:on("release", function(event)
    local press = folder_press
    folder_press = nil
    if not press or event.button ~= "left" or press.tree ~= tree then return end
    local row = pointer_row(event)
    sidebar:cursor(press.row)
    if row == press.row then toggle_directory() end
  end)
  sidebar:on("scrolled", function() folder_press = nil end)
  sidebar:on("resized", function() folder_press = nil end)
  preview:on("scrolled", sync_sidebar)
  preview:on("selection_changed", sync_sidebar)

  local function nav_file(delta)
    if browsing then
      sidebar:cursor(math.max(0, math.min(#entries - 1, (sidebar:cursor() or 0) + delta)))
      select_commit()
      return
    end
    local node = (focused == "files" or not selected) and selected_node() or nil
    if node and (node.children or node.group) then
      jump_file(delta > 0 and node.first or node.first - 1)
    else
      jump_file((node and node.index or selected or 1) + delta)
    end
  end
  local function nav_tree(delta)
    if browsing then nav_file(delta); return end
    if not tree then return end
    local last = tree:document():row_count() - 1
    local current = sidebar:cursor() or 0
    local row = current + delta
    local separator = view == "changes" and (tree:section_row("staged") - 1) or -1
    if current < separator and row >= separator then
      row = row + 1
    elseif current > separator and row <= separator then
      row = row - 1
    end
    sidebar:cursor(math.max(0, math.min(last, row)))
    select_file()
  end
  local function open_node()
    if browsing then
      local entry = entries[(sidebar:cursor() or 0) + 1]
      if entry and entry.more then load_history(true); return end
      if not diff or preview_job or list_error then return end
      list_row, list_top = sidebar:cursor() or 0, (sidebar:scroll() or {}).top or 0
      browsing = false
      build_tree()
      if file_count > 0 then jump_file(1) end
      status()
      focus_pane("files")
      return
    end
    local node = selected_node()
    if not node then return end
    if node.children then
      toggle_directory()
    elseif node.index then
      jump_file(node.index)
      focus_pane("preview")
    end
  end
  local function left()
    if browsing then return end
    local node = selected_node()
    if not node then return end
    if node.children and node.expanded then
      toggle_directory()
    elseif node.parent then
      sidebar:cursor(node.parent)
    end
  end
  local function right()
    local node = selected_node()
    if node and node.children then
      if node.expanded then nav_tree(1) else toggle_directory() end
    else
      open_node()
    end
  end
  local function sidebar_key(key, action)
    sidebar:key(key, function()
      local count = sidebar_count or 1
      sidebar_count, folder_press = nil, nil
      action(count)
    end)
  end
  for digit = 0, 9 do
    sidebar:key(tostring(digit), function()
      folder_press = nil
      if (tree or browsing) and (sidebar_count or digit > 0) then
        sidebar_count = math.min((sidebar_count or 0) * 10 + digit, browsing and #entries or tree:document():row_count())
      end
    end)
  end
  sidebar_key("ctrl-j", function() nav_file(1) end)
  sidebar_key("ctrl-k", function() nav_file(-1) end)
  for _, binding in ipairs({
    { "j", 1 }, { "k", -1 }, { "down", 1 }, { "up", -1 },
    { "ctrl-n", 1 }, { "ctrl-p", -1 },
  }) do
    sidebar_key(binding[1], function(count) nav_tree(binding[2] * count) end)
  end
  for _, key in ipairs({ "g", "home" }) do
    sidebar_key(key, function() sidebar:cursor(0); select_file() end)
  end
  for _, key in ipairs({ "G", "end" }) do
    sidebar_key(key, function()
      if tree or browsing then
        sidebar:cursor(math.max(0, (browsing and #entries or tree:document():row_count()) - 1))
        select_file()
      end
    end)
  end
  sidebar_key("enter", open_node)
  sidebar_key("space", open_node)
  sidebar_key("h", left)
  sidebar_key("left", left)
  sidebar_key("l", right)
  sidebar_key("right", right)
  for _, binding in ipairs({
    { "ctrl-u", -0.5 }, { "ctrl-d", 0.5 },
    { "ctrl-b", -1 }, { "ctrl-f", 1 }, { "pgup", -1 }, { "pgdn", 1 },
  }) do
    sidebar_key(binding[1], function(count)
      local height = (sidebar:rect() or {}).height or 10
      local delta = math.max(1, math.floor(height * math.abs(binding[2]))) * count
      nav_tree(binding[2] < 0 and -delta or delta)
    end)
  end

  local function hunk(forward)
    if not diff or not selected then return end
    local row = diff:hunk(preview:cursor() or 0, forward)
    if row then preview:scroll(row); preview:cursor(row); focus_pane("preview") end
  end
  local function fold()
    if not diff or not selected then return end
    local row = diff:toggle_fold(preview:cursor() or 0)
    if row then preview:scroll(row); preview:cursor(row) end
  end
  preview:key("enter", fold)
  preview:key("H", function() preview:pan(-4) end)
  preview:key("L", function() preview:pan(4) end)
  preview:key("<S-Left>", function() preview:pan(-4) end)
  preview:key("<S-Right>", function() preview:pan(4) end)

  local function install(result, keep, next_file, restore_position)
    local previous_tree = tree
    local sidebar_scroll = tree and sidebar:scroll()
    local sidebar_row = sidebar:cursor() or 0
    local sidebar_offset = sidebar_scroll and (sidebar_row - sidebar_scroll.top)
    local scroll = preview:scroll() or {}
    local cursor, top
    if diff then cursor, top = result.diff:restore(diff, preview:cursor() or 0, scroll.top or 0) end
    local pan = scroll.left or 0
    diff, selected = result.diff, nil
    if view ~= "changes" then
      local entry = entries[list_row + 1]
      comparison_label = entry and entry.combined and (display(base) .. "..." .. head:sub(1, 8)) or display(result.branch)
    end
    stale = false
    file_count = diff:file_count()
    build_tree()
    local group_row = restore_position and previous_tree and tree:restore_group(previous_tree, sidebar_row)
    local index = keep and diff:find_file(keep.section, keep.key)
    local same_file = index ~= nil
    if not index and next_file then index = diff:find_file(next_file.section, next_file.key) end
    if not index then
      local first, last = diff:section_range(keep and keep.section or (view == "changes" and "unstaged" or "changes"))
      if first <= last then index = first elseif not keep and file_count > 0 then index = 1 end
    end
    if index then
      jump_file(index, group_row)
      if same_file and restore_position then
        if cursor then preview:cursor(cursor) end
        if top then preview:scroll(top) end
      end
      preview:pan(pan)
    else
      local section = keep and keep.section or (view == "changes" and "unstaged" or "changes")
      sidebar:cursor(tree:section_row(section))
      notice(view ~= "changes" and "No changes" or (file_count == 0 and "Working tree clean" or
        (section == "unstaged" and "Everything is staged" or "Nothing staged")), "r refresh")
    end
    if group_row then sidebar:cursor(group_row) end
    if sidebar_offset then
      sidebar:scroll(math.max(0, (sidebar:cursor() or 0) - sidebar_offset))
    end
  end

  local function no_commits()
    return view == "branch" and ("Up to date with " .. display(base or "the base")) or "No commits"
  end

  local function render_commits()
    entries = {}
    local rows = {}
    local width = sidebar:content_width() or 38
    if view == "branch" and head and #commits > 0 then
      entries[#entries + 1] = { combined = true }
      local add = branch_totals and branch_totals[1] > 0 and (" +" .. branch_totals[1]) or ""
      local del = branch_totals and branch_totals[2] > 0 and (" -" .. branch_totals[2]) or ""
      local marker = branch_totals and branch_totals[3] and " binary" or ""
      rows[#rows + 1] = {
        { text = smelt.text.truncate_cells("Combined changes vs " .. display(base), math.max(1, width - #add - #del - #marker)), style = "SmeltAccent" },
        { text = add, style = "SmeltDiffAddCount" },
        { text = del, style = "SmeltDiffDeleteCount" },
        { text = marker, style = "Comment" },
      }
    end
    for _, commit in ipairs(commits) do
      entries[#entries + 1] = commit
      local add = commit.additions > 0 and (" +" .. commit.additions) or ""
      local del = commit.deletions > 0 and (" -" .. commit.deletions) or ""
      local marker = commit.binary and " binary" or ((add == "" and del == "") and " empty" or "")
      local suffix = #add + #del + #marker
      rows[#rows + 1] = {
        { text = commit.id:sub(1, 7) .. " ", style = "Comment" },
        { text = smelt.text.fit(display(commit.subject), math.max(1, width - 8 - suffix)) },
        { text = add, style = "SmeltDiffAddCount" },
        { text = del, style = "SmeltDiffDeleteCount" },
        { text = marker, style = "Comment" },
      }
    end
    if more then
      entries[#entries + 1] = { more = true }
      rows[#rows + 1] = { { text = "Load older commits (Enter)", style = "Comment" } }
    end
    sidebar:document(nil)
    sidebar_buf:styled(#rows > 0 and rows or { { { text = list_error or no_commits(), style = "Comment" } } })
  end

  select_commit = function(force)
    if loading then return end
    local row = sidebar:cursor() or 0
    local entry = entries[row + 1]
    if not force and row == list_row and diff then return end
    list_row = row
    if preview_job then preview_job:remove(); preview_job = nil end
    diff, selected, tree, file_count = nil, nil, nil, 0
    preview:document(nil)
    preview:scroll(0)
    preview:cursor(0)
    if not entry or entry.more then
      info_buf:lines({ "" })
      notice(entry and "Older commits" or (list_error or no_commits()), entry and "Enter load" or nil)
      return
    end
    revision = entry.combined and head or entry.id
    comparison_label = entry.combined and (display(base) .. "...HEAD") or (entry.id:sub(1, 8) .. " - first parent")
    local details = { "Branch changes vs " .. display(base), "Committed changes only", #commits .. (more and "+ commits" or (#commits == 1 and " commit" or " commits")) }
    if not entry.combined then
      details = { entry.id:sub(1, 8) .. " | " .. display(entry.author) .. " | " .. display(entry.age) }
      for line in (entry.message .. "\n"):gmatch("(.-)\n") do details[#details + 1] = display(line) end
    end
    info_buf:lines(details)
    info:scroll(0)
    notice("Loading")
    status()
    local target, target_base = revision, entry.combined and base_id or nil
    preview_job = smelt.spawn(function()
      local result, err = smelt.git.diff({ cwd = cwd, revision = target, base = target_base })
      if closed then return end
      preview_job = nil
      if not result then
        notice("Comparison failed: " .. display(err), "r retry")
      else
        diff = result.diff
        file_count = diff:file_count()
        comparison_label = entry.combined and (display(base) .. "..." .. head:sub(1, 8)) or display(result.branch)
        selected = file_count > 0 and 1 or nil
        if entry.combined then
          branch_totals = { diff:totals() }
          render_commits()
          sidebar:cursor(list_row)
        end
        if file_count > 0 then preview:document(diff:document()) else notice("No changes") end
      end
      status()
    end)
  end

  show_commits = function()
    if view == "changes" then return end
    if loading then loading:remove(); loading = nil end
    busy = nil
    browsing = true
    tree, selected = nil, nil
    render_commits()
    sidebar:cursor(list_row)
    sidebar:scroll(list_top)
    status()
    focus_pane("files")
  end

  load_history = function(append)
    if changing then return end
    stop_reads()
    busy = "loading commits"
    browsing = true
    if not append then branch_totals = nil end
    status()
    loading = smelt.spawn(function()
      local page, err = smelt.git.history({ cwd = cwd, branch = view == "branch", base = append and base_id or base,
        head = append and head or nil, skip = append and #commits or 0 })
      if closed then return end
      loading, busy = nil, nil
      if not page then
        if append then
          smelt.notify.error("History load failed: " .. display(err))
        else
          list_error = display(err)
          commits, head, more, diff, tree, selected = {}, nil, false, nil, nil, nil
          render_commits()
          info_buf:lines({ "History unavailable" })
          notice(list_error, "Alt-b base  r retry")
        end
        status()
        return
      end
      list_error = nil
      if not append then commits = {} end
      for _, commit in ipairs(page.commits) do commits[#commits + 1] = commit end
      head, more = page.head, page.more
      if not append and page.base then base = page.base end
      base_id = page.base_id
      diff, tree, selected = nil, nil, nil
      render_commits()
      sidebar:cursor(math.min(list_row, math.max(0, #entries - 1)))
      select_commit(true)
      status()
    end)
  end

  local function refresh()
    if browsing then load_history(false); return end
    sidebar_count, folder_press = nil, nil
    if changing then return end
    if loading then loading:remove() end
    busy = diff and "refreshing" or "loading"
    status()
    loading = smelt.spawn(function()
      local entry = entries[list_row + 1]
      local result, err = smelt.git.diff({ cwd = cwd, revision = view ~= "changes" and revision or nil,
        base = view ~= "changes" and entry and entry.combined and base_id or nil })
      if closed then return end
      loading, busy = nil, nil
      if not result then
        if diff then
          stale = true
          smelt.notify.error("refresh failed: " .. display(err))
        else
          notice("Load failed: " .. display(err or "git diff failed"), "r retry")
          build_tree()
        end
      else
        local keep = selected and diff:file(selected)
        install(result, keep, nil, true)
      end
      status()
    end)
  end

  local function change_index(action)
    sidebar_count, folder_press = nil, nil
    if view ~= "changes" or changing or loading or stale or not diff then return end
    if focused == "preview" and smelt.vim.mode() ~= "normal" then return end
    local node = focused == "files" and selected_node() or nil
    local index = node and node.first or (focused == "preview" and selected)
    local finish = node and node.last or index
    if not index or index > finish then return end
    local keep = diff:file(index)
    if (action == "stage" and keep.section ~= "unstaged") or
       (action == "unstage" and keep.section ~= "staged") then return end
    if not node or node.index then reveal_file(index) end
    local first, last = diff:section_range(keep.section)
    local next_file = finish < last and diff:file(finish + 1) or (index > first and diff:file(index - 1) or nil)
    local operation = keep.section == "unstaged" and "stage" or "unstage"
    busy = operation == "stage" and "staging" or "unstaging"
    status()
    changing = smelt.spawn(function()
      local result, err = smelt.git.index(diff, index, action, finish)
      if closed then return end
      changing, busy = nil, nil
      if not result then
        stale = true
        smelt.notify.error(operation .. " failed: " .. display(err))
      elseif result.refresh_error then
        stale = true
        smelt.notify.error(operation .. " succeeded; refresh failed: " .. display(result.refresh_error))
      else
        install(result, keep, next_file)
      end
      status()
    end)
  end

  local function cycle_view(step)
    for index, tab in ipairs(views) do
      if tab == view then switch_view(views[(index - 1 + step) % #views + 1]); return end
    end
  end

  switch_view = function(next_view)
    if changing or next_view == view then return end
    query_open = false
    stop_reads()
    saved[view] = { diff = diff, tree = tree, selected = selected, browsing = browsing,
      commits = commits, entries = entries, head = head, more = more, revision = revision, base_id = base_id,
      row = sidebar:cursor() or 0, top = (sidebar:scroll() or {}).top or 0,
      list_row = list_row, list_top = list_top, label = comparison_label,
      preview_cursor = preview:cursor() or 0, preview_top = (preview:scroll() or {}).top or 0,
      info = info_buf:lines(), stale = stale, branch_totals = branch_totals }
    view, busy, list_error = next_view, nil, nil
    local state = saved[view]
    saved[view] = nil
    diff, tree, selected, file_count = nil, nil, nil, 0
    comparison_label, revision, head, more, stale = nil, nil, nil, false, false
    commits, entries, list_row, list_top = {}, {}, 0, 0
    browsing = view ~= "changes"
    sidebar:document(nil)
    notice("Loading")
    if state then
      diff, tree, selected, browsing = state.diff, state.tree, state.selected, state.browsing
      commits, entries, head, more = state.commits, state.entries, state.head, state.more
      revision, list_row, list_top, comparison_label = state.revision, state.list_row, state.list_top, state.label
      stale, branch_totals, base_id = state.stale, state.branch_totals, state.base_id
      file_count = diff and diff:file_count() or 0
      info_buf:lines(state.info)
      if browsing then render_commits() elseif tree then sidebar:document(tree:document()) end
      sidebar:cursor(state.row)
      sidebar:scroll(state.top)
      if diff and file_count > 0 then
        preview:document(diff:document())
        preview:cursor(state.preview_cursor)
        preview:scroll(state.preview_top)
      elseif browsing then
        if head or #entries > 0 then select_commit(true) else load_history(false) end
      else refresh() end
      status()
    elseif browsing then load_history(false) else refresh() end
    focus_pane(browsing and "files" or "preview")
  end

  local function edit_query(purpose)
    if changing then return end
    input_purpose = purpose
    input_buf:lines({ purpose == "base" and (base or "") or "" })
    input:placeholder(purpose == "base" and "Comparison base (Enter)" or "Commit ID or revision (Enter)", { dismiss_keys = {} })
    query_open = true
    status()
    input:focus()
  end
  input:key("enter", function()
    local value = (input_buf:line(1) or ""):match("^%s*(.-)%s*$")
    if value == "" or changing then return end
    query_open = false
    if input_purpose == "base" then
      base = value
      saved.branch = nil
      if view ~= "branch" then switch_view("branch") else load_history(false) end
      focus_pane("files")
    else
      if view ~= "history" then switch_view("history") end
      stop_reads()
      browsing, revision, comparison_label = false, value, display(value) .. " - first parent"
      entries, diff, tree, selected = {}, nil, nil, nil
      revision_info(value)
      refresh()
      focus_pane("files")
    end
    input_buf:lines({ "" })
    input_purpose = "revision"
    input:placeholder("Commit ID or revision", { dismiss_keys = {} })
  end)
  input:key("esc", function()
    query_open = false
    input_buf:lines({ "" })
    focus_pane("files")
    status()
  end)
  local function on_terminal_resize()
    local narrow = fits_one_pane()
    if narrow == single_pane then return end
    single_pane = narrow
    status()
    focus_pane(focused)
  end
  preview:on("resized", on_terminal_resize)
  sidebar:on("resized", function(event)
    on_terminal_resize()
    if browsing then render_commits() end
    -- The title runs from after the corner to one cell before the divider.
    local compact = event.width - 1 < tabs_width
    if compact ~= compact_tabs then
      compact_tabs = compact
      status()
    end
  end)

  overlay_opts.keymaps = {
      { key = "alt-1", on_press = function() switch_view("changes") end },
      { key = "alt-2", on_press = function() switch_view("branch") end },
      { key = "alt-3", on_press = function() switch_view("history") end },
      { key = "backspace", on_press = function()
        if view ~= "changes" and not browsing then
          if #entries == 0 then load_history(false) else show_commits() end
        end
      end },
      { key = "alt-w", on_press = function()
        if single_pane then focus_pane(focused == "files" and "preview" or "files") end
      end },
      { key = "alt-r", on_press = function() edit_query("revision") end },
      { key = "alt-b", on_press = function() edit_query("base") end },
      { key = "q", on_press = close },
      { key = "esc", on_press = close },
      { key = "ctrl-c", on_press = close },
      { key = "r", on_press = refresh },
      { key = "ctrl-j", on_press = function() nav_file(1) end },
      { key = "ctrl-k", on_press = function() nav_file(-1) end },
      { key = "s", on_press = function() change_index("stage") end },
      { key = "u", on_press = function() change_index("unstage") end },
      { key = "-", on_press = function() change_index("toggle") end },
      { key = "tab", on_press = function() cycle_view(1) end },
      { key = "<S-Tab>", on_press = function() cycle_view(-1) end },
      { key = "{", on_press = function() hunk(false) end },
      { key = "}", on_press = function() hunk(true) end },
      { key = "[", on_press = function() nav_file(-1) end },
      { key = "]", on_press = function() nav_file(1) end },
  }
  local shared_keys, view_keys, view_regs = {}, {}, {}
  for _, binding in ipairs(overlay_opts.keymaps) do
    if binding.key:match("^alt%-") or binding.key == "esc" or binding.key == "ctrl-c" or
        binding.key == "tab" or binding.key == "<S-Tab>" then
      shared_keys[#shared_keys + 1] = binding
    else
      view_keys[#view_keys + 1] = binding
    end
  end
  overlay_opts.keymaps = shared_keys
  status()
  overlay_opts.keymaps = nil
  local function view_shortcuts(enabled)
    for _, reg in ipairs(view_regs) do reg:remove() end
    view_regs = {}
    if enabled and not closed then
      for _, binding in ipairs(view_keys) do
        view_regs[#view_regs + 1] = overlay:key(binding.key, binding.on_press)
      end
    end
  end
  -- Keep viewer shortcuts below Vim's pending motions, but out of text input.
  input:on("focus", function() view_shortcuts(false) end)
  input:on("blur", function() view_shortcuts(true) end)
  view_shortcuts(true)
  active = { close = close }
  if revision then
    revision_info(revision)
  end
  notice("Loading")
  if browsing then
    focus_pane("files")
    load_history(false)
  else
    preview:focus()
    if retained then
      local previous = retained
      retained = nil
      previous.timer:remove()
      if previous.cwd == cwd then install({ diff = previous.diff }) end
    end
    refresh()
  end
end

smelt.cmd.register("diff", function(arg)
  local word, value = (arg or ""):match("^(%S+)%s*(.-)%s*$")
  if not word then M.open()
  elseif word == "branch" then M.open({ view = "branch", base = value ~= "" and value or nil })
  elseif word == "history" then M.open({ view = "history" })
  else M.open({ view = "history", revision = arg }) end
end, {
  desc = "review working changes, branch commits, history, or one revision",
  args = { "[branch [base]|history|revision]" }, busy = "run",
})

return M
