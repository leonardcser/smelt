-- Built-in /resume command. Centered Telescope-style overlay: results and
-- search input on the left, rendered transcript preview on the right. Arrows +
-- Ctrl-J/K navigate; typing into the input filters; Enter loads; Alt-D deletes
-- the highlighted session; Ctrl-W toggles the workspace filter between "this
-- workspace" (default) and "all sessions".
--
-- Matching is two-tier:
--   * Title + first-user-message: fuzzy match (`smelt.fuzzy`), instant - runs
--     against cheap meta loaded up front.
--   * Full message text: substring match against canonical SQLite search text,
--     loaded in parallel on the first non-empty query and cached for the dialog
--     lifetime. Opening the dialog stays instant; the first keystroke pays the
--     IO cost once.

local NS_STATE = smelt.ns("smelt.resume.state")

local function is_junk(s)
  if s == nil then return true end
  local t = s:match("^%s*(.-)%s*$") or ""
  if t == "" then return true end
  if t:lower() == "untitled" then return true end
  local first = t:sub(1, 1)
  if first == "/" or first == "\0" then return true end
  return false
end

local function display_title(entry)
  local raw
  if not is_junk(entry.title) then
    raw = entry.title
  elseif not is_junk(entry.subtitle) then
    raw = entry.subtitle
  end
  local title = "Untitled"
  if raw ~= nil then
    local line = raw:match("([^\n]*)") or raw
    title = line:match("^%s*(.-)%s*$") or line
  end
  return title
end

local SIZE_WIDTH = 6

local function format_size(bytes)
  if bytes == nil or bytes <= 0 then return "" end
  if bytes < 1024 then return string.format("%dB", bytes) end

  local units = { "K", "M", "G", "T", "P" }
  local value = bytes / 1024
  local unit_idx = 1
  local text = string.format("%.1f%s", value, units[unit_idx])
  while #text > SIZE_WIDTH and unit_idx < #units do
    value = value / 1024
    unit_idx = unit_idx + 1
    text = string.format("%.1f%s", value, units[unit_idx])
  end
  if #text > SIZE_WIDTH then
    text = string.format("%.0f%s", value, units[unit_idx])
  end
  return text
end

local function time_ago(ts_ms, now_ms)
  if ts_ms == nil or ts_ms <= 0 then return "" end
  local delta = math.max(0, (now_ms - ts_ms) / 1000)
  if delta < 60 then return string.format("%ds", math.floor(delta)) end
  if delta < 3600 then return string.format("%dm", math.floor(delta / 60)) end
  if delta < 86400 then return string.format("%dh", math.floor(delta / 3600)) end
  return string.format("%dd", math.floor(delta / 86400))
end

local LEADING, SIZE_COL, TIME_COL, GAP = 1, SIZE_WIDTH, 4, 1
local PREVIEW_MIN_TERM_WIDTH = 100

local UNAVAILABLE_LABELS = {
  missing_database = "missing database",
  symlink_not_allowed = "unsafe symlink",
  unsupported_schema = "unsupported schema",
  corrupt = "corrupt session",
  io = "I/O error",
  sqlite = "database error",
}

local function unavailable_reason(entry)
  return UNAVAILABLE_LABELS[entry.error_kind] or "storage error"
end

local function make_render(now_ms)
  return function(entry)
    local prefix = entry.tree_prefix or ""
    if entry.available == false then
      local meta = string.rep(" ", LEADING + SIZE_COL + GAP + TIME_COL + GAP)
      return {
        text = meta .. prefix .. "Unavailable - " .. unavailable_reason(entry),
        marks = { { col = 0, opts = { end_col = #meta + #prefix, dim = true } } },
      }
    end
    local size_str = format_size(entry.size_bytes)
    local ts = (entry.updated_at_ms > 0) and entry.updated_at_ms or entry.created_at_ms
    local meta = string.format(
      "%s%" .. SIZE_COL .. "s%s%-" .. TIME_COL .. "s%s",
      string.rep(" ", LEADING),
      size_str,
      string.rep(" ", GAP),
      time_ago(ts, now_ms),
      string.rep(" ", GAP)
    )
    local marks = { { col = 0, opts = { end_col = #meta, dim = true } } }
    if prefix ~= "" then
      marks[#marks + 1] = {
        col = #meta,
        opts = { end_col = #meta + #prefix, dim = true },
      }
    end
    return {
      text  = meta .. prefix .. display_title(entry),
      marks = marks,
    }
  end
end

local function update_state_label(buf, workspace_only)
  buf:clear_ns(NS_STATE)
  local scope = workspace_only and "workspace" or "all"
  local label = " ⌥d: delete · " .. scope .. " "
  buf:mark(NS_STATE, 1, 0, {
    virt_text     = label,
    virt_text_pos = "right_align",
    dim           = true,
  })
end

-- Wire the `--resume` CLI flag (declared in `smelt/early/resume.lua`) to a
-- startup action: nil = no flag, "" = open picker, else = load that session.
-- Gated on `ctx.kind == "launch"` because the module body re-runs (and
-- "ready" hooks re-drain) on every `/reload`; without the gate, a reload
-- would re-open the picker every time.
smelt.lifecycle.on_ready(function(ctx)
  if ctx.kind ~= "launch" then return end
  local v = smelt.cli.get("resume")
  if v == nil then return end
  if v == "" then
    smelt.cmd.run("resume")
  else
    __smelt_internal.session.__load_now(v)
  end
end)

smelt.cmd.register("resume", function()
  smelt.spawn(function()
    local entries = {}
    local cursor = nil
    local catalog_state = "ready"
    repeat
      local page = smelt.session.list({ limit = 500, cursor = cursor })
      catalog_state = page.catalog.state
      for _, entry in ipairs(page.entries) do table.insert(entries, entry) end
      cursor = page.next_cursor
    until cursor == nil
    if #entries == 0 then
      if catalog_state == "reconciling" then
        smelt.notify.error("session catalog is rebuilding; try again shortly")
      elseif catalog_state == "degraded" then
        smelt.notify.error("session catalog is unavailable")
      else
        smelt.notify.error("no saved sessions")
      end
      return
    end

    local current_cwd    = smelt.session.cwd()
    local now_ms         = os.time() * 1000
    local workspace_only = true
    local query          = ""

    -- Pre-build title haystacks once. Hot path runs `smelt.fuzzy.score` against
    -- these per refilter; rebuilding per call would re-concatenate strings.
    local title_hays = {}
    for _, e in ipairs(entries) do
      title_hays[e.id] = table.concat({
        display_title(e),
        e.subtitle or "",
        e.error or "",
      }, " ")
    end

    -- Lowercased content blobs, lazy-loaded in parallel on the first non-empty
    -- query. Stays `nil` while the user is just toggling the workspace filter.
    local texts = nil

    local function ensure_texts()
      if texts ~= nil then return end
      local ids = {}
      for _, e in ipairs(entries) do
        if e.available ~= false then table.insert(ids, e.id) end
      end
      local raw = smelt.session.texts(ids)
      local lowered = {}
      for k, v in pairs(raw) do lowered[k] = v:lower() end
      texts = lowered
    end

    local function entry_matches(entry)
      if entry.available ~= false and workspace_only and entry.cwd ~= current_cwd then return false end
      if query == "" then return true end
      if smelt.fuzzy.score(title_hays[entry.id] or "", query) ~= nil then
        return true
      end
      local blob = texts and texts[entry.id]
      return blob ~= nil and blob:find(query:lower(), 1, true) ~= nil
    end

    local function visible_tree()
      local visible = {}
      for _, e in ipairs(entries) do
        if entry_matches(e) then table.insert(visible, e) end
      end
      return smelt.session.tree(visible, { order = "asc" })
    end

    local task_id = smelt.task.alloc()
    local resolved = false
    local overlay

    local input_buf = smelt.buf.new()
    input_buf:lines({ "" })
    local input_leaf = smelt.win.new(input_buf, {
      region = "resume_overlay", surface = "editable_text",
      pad_left = 1, pad_right = 1, scrollbar = false, wrap = false,
      kind = "input", placeholder = "filter sessions…",
    })

    local list_buf = smelt.buf.new()
    local list_leaf = smelt.win.new(list_buf, {
      region = "resume_overlay", surface = "list_inert",
      pad_left = 1, pad_right = 1, scrollbar = false,
      kind = "list", initial_cursor = 0,
    })

    local ui_size = smelt.ui.size()
    local show_preview = (ui_size.width or 80) >= PREVIEW_MIN_TERM_WIDTH
    local preview_buf, preview_leaf
    if show_preview then
      preview_buf = smelt.buf.new({ readonly = true })
      preview_buf:lines({ "" })
      preview_leaf = smelt.win.new(preview_buf, {
        region = "resume_overlay", surface = "selectable_text",
        pad_left = 0, pad_right = 0, wrap = false, scrollbar = true,
      })
    end

    local preview_timer = nil

    local list = smelt.list.new({
      leaf       = list_leaf,
      buf        = list_buf,
      items      = visible_tree(),
      render     = make_render(now_ms),
      anchor     = "bottom",
      empty_text = "  (no matching sessions)",
    })

    local function select_bottom()
      local n = list:size()
      if n > 0 then
        list:set_cursor(n - 1)
        list_leaf:scroll("tail")
      end
    end

    local function preview_size()
      if not preview_leaf then return 80, 1 end
      local width = preview_leaf:content_width() or 80
      local rect = preview_leaf:rect() or {}
      local height = math.max(1, rect.height or 1)
      return width, height
    end

    local function render_preview()
      if preview_timer then
        preview_timer:remove()
        preview_timer = nil
      end
      if not show_preview then return end
      local e = list:selected()
      if not e then
        preview_buf:lines({ "  (no session selected)" })
        preview_leaf:scroll(0)
        return
      end
      if e.available == false then
        preview_buf:lines({
          "  Session unavailable: " .. unavailable_reason(e),
          "",
          "  " .. (e.error or "No storage details are available."),
          "",
          "  Press Alt-D to remove this session.",
        })
        preview_leaf:scroll(0)
        return
      end
      local width, height = preview_size()
      smelt.session.render_preview_into(e.id, {
        buf = preview_buf,
        win = preview_leaf,
        width = width,
        height = height,
        updated_at_ms = e.updated_at_ms,
      })
    end

    local function schedule_preview()
      if not show_preview then return end
      if preview_timer then preview_timer:remove() end
      preview_timer = smelt.timer.set(40, function()
        preview_timer = nil
        if resolved then return end
        render_preview()
      end)
    end

    local function refilter()
      list:set_items(visible_tree())
      select_bottom()
      schedule_preview()
    end

    local function close(value)
      if resolved then return end
      resolved = true
      if preview_timer then
        preview_timer:remove()
        preview_timer = nil
      end
      if overlay then overlay:close() end
      smelt.task.resume(task_id, value)
    end

    local function submit()
      local e = list:selected()
      if not e then return end
      if e.available == false then
        smelt.notify.error(e.error or "session unavailable")
        return
      end
      close(e)
    end

    update_state_label(input_buf, workspace_only)

    input_leaf:on("text_changed", function(raw)
      query = (raw and raw.text) or ""
      if query ~= "" then ensure_texts() end
      refilter()
    end)
    list_leaf:on("selection_changed", function() schedule_preview() end)
    list_leaf:on("resized", function() select_bottom() end)
    if show_preview then
      preview_leaf:on("resized", function() schedule_preview() end)
    end
    input_leaf:on("submit", function() submit() end)
    list_leaf:on("submit", function() submit() end)
    input_leaf:on("dismiss", function() close(nil) end)
    list_leaf:on("dismiss", function() close(nil) end)
    if show_preview then preview_leaf:on("dismiss", function() close(nil) end) end

    local function nav(delta)
      return function()
        list:move_cursor(delta)
        schedule_preview()
      end
    end

    local function delete_selected()
      local e = list:selected()
      if not e then return end
      smelt.session.delete(e.id)
      for i, x in ipairs(entries) do
        if x.id == e.id then table.remove(entries, i); break end
      end
      title_hays[e.id] = nil
      if texts then texts[e.id] = nil end
      refilter()
    end

    local function toggle_workspace()
      workspace_only = not workspace_only
      update_state_label(input_buf, workspace_only)
      refilter()
    end

    input_leaf:key("up",     nav(-1))
    input_leaf:key("down",   nav(1))
    input_leaf:key("ctrl-k", nav(-1))
    input_leaf:key("ctrl-j", nav(1))
    input_leaf:key("ctrl-p", nav(-1))
    input_leaf:key("ctrl-n", nav(1))
    input_leaf:key("pgup",   nav(-10))
    input_leaf:key("pgdn",   nav(10))
    input_leaf:key("ctrl-u", nav(-5))
    input_leaf:key("ctrl-d", nav(5))
    input_leaf:key("ctrl-w", toggle_workspace)
    input_leaf:key("alt-d",  delete_selected)

    local list_layout = smelt.ui.layout.leaf(list_leaf, {
      border = { all = "Comment" },
      title = smelt.dialog.title(" sessions "),
    })
    local input_layout = smelt.ui.layout.leaf(input_leaf, {
      border = { all = "Comment" },
      title = smelt.dialog.title(" filter "),
    })
    local left = smelt.ui.layout.vbox({
      { list_layout,  height = "fill" },
      { input_layout, height = 3      },
    }, { gap = 0 })
    local root = left
    if show_preview then
      root = smelt.ui.layout.hbox({
        { left, width = "40%" },
        { smelt.ui.layout.leaf(preview_leaf, {
            border = { all = "Comment" },
            title = smelt.dialog.title(" transcript "),
          }), width = "fill" },
      }, { gap = 0, padding = 0 })
    end

    overlay = smelt.overlay.new({
      anchor = "center",
      border = "none",
      modal  = true,
      width  = "85%",
      height = "75%",
      layout = root,
      keymaps = {
        { key = "up",     on_press = nav(-1) },
        { key = "down",   on_press = nav(1)  },
        { key = "ctrl-k", on_press = nav(-1) },
        { key = "ctrl-j", on_press = nav(1)  },
        { key = "ctrl-p", on_press = nav(-1) },
        { key = "ctrl-n", on_press = nav(1)  },
        { key = "pgup",   on_press = nav(-10) },
        { key = "pgdn",   on_press = nav(10)  },
        { key = "ctrl-u", on_press = nav(-5)  },
        { key = "ctrl-d", on_press = nav(5)   },
        { key = "enter",  on_press = submit },
        { key = "esc",    on_press = function() close(nil) end },
        { key = "c-c",    on_press = function() close(nil) end },
        { key = "ctrl-w", on_press = toggle_workspace },
        { key = "alt-d",  hint = "⌥d: delete", on_press = delete_selected },
      },
    })
    list:refresh()
    select_bottom()
    render_preview()
    input_leaf:focus()

    local picked = smelt.task.wait(task_id)
    if picked then smelt.session.load(picked.id) end
  end)
end, { desc = "resume saved session", busy = "reject", startup_ok = true })
