-- Built-in /resume command. Telescope-style picker: search input on top, results
-- below. Arrows + Ctrl-J/K navigate; typing into the input filters; Enter loads;
-- Ctrl-D deletes the highlighted session; Alt-W toggles the workspace filter.

local NS_META = smelt.buf.create_namespace("smelt.resume.meta")

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
  else
    return "Untitled"
  end
  local line = raw:match("([^\n]*)") or raw
  return (line:match("^%s*(.-)%s*$") or line)
end

local function format_size(bytes)
  if bytes == nil or bytes <= 0 then return "" end
  if bytes < 1024 then return string.format("%dB", bytes) end
  if bytes < 1024 * 1024 then return string.format("%.1fK", bytes / 1024) end
  return string.format("%.1fM", bytes / 1024 / 1024)
end

local function time_ago(ts_ms, now_ms)
  if ts_ms == nil or ts_ms <= 0 then return "" end
  local delta = math.max(0, (now_ms - ts_ms) / 1000)
  if delta < 60 then return string.format("%ds", math.floor(delta)) end
  if delta < 3600 then return string.format("%dm", math.floor(delta / 60)) end
  if delta < 86400 then return string.format("%dh", math.floor(delta / 3600)) end
  return string.format("%dd", math.floor(delta / 86400))
end

local LEADING, SIZE_COL, TIME_COL, GAP = 2, 8, 7, 2

local function format_row(entry, now_ms)
  local size_str = format_size(entry.size_bytes)
  local time_str = time_ago(
    (entry.updated_at_ms > 0) and entry.updated_at_ms or entry.created_at_ms,
    now_ms
  )
  return string.format(
    "%s%" .. SIZE_COL .. "s%s%-" .. TIME_COL .. "s%s%s",
    string.rep(" ", LEADING),
    size_str,
    string.rep(" ", GAP),
    time_str,
    string.rep(" ", GAP),
    display_title(entry)
  )
end

local function filter_entries(entries, query, workspace_only, current_cwd)
  local out = {}
  for _, e in ipairs(entries) do
    local keep = true
    if workspace_only then keep = (e.cwd == current_cwd) end
    if keep and query ~= "" then
      local hay = display_title(e) .. " " .. (e.subtitle or "")
      keep = smelt.fuzzy.score(hay, query) ~= nil
    end
    if keep then table.insert(out, e) end
  end
  return out
end

local function refresh_list(list_buf, filtered, now_ms)
  -- NS_META is a custom namespace, so `set_lines` doesn't clear it for us;
  -- wipe it ourselves before each render or stale dim marks from a longer
  -- previous list leak into shorter renders (e.g. the empty-state line).
  smelt.buf.clear_namespace(list_buf, NS_META)
  if #filtered == 0 then
    local empty = "  (no matching sessions)"
    smelt.buf.set_lines(list_buf, { empty })
    smelt.buf.set_extmark(list_buf, NS_META, 1, 0, { end_col = #empty, dim = true })
    return
  end
  local lines = {}
  for _, e in ipairs(filtered) do table.insert(lines, format_row(e, now_ms)) end
  smelt.buf.set_lines(list_buf, lines)
  local meta_end = LEADING + SIZE_COL + GAP + TIME_COL
  for i = 1, #filtered do
    smelt.buf.set_extmark(list_buf, NS_META, i, 0, { end_col = meta_end, dim = true })
  end
end

smelt.cmd.register("resume", function()
  smelt.spawn(function()
    local entries = smelt.session.list()
    if #entries == 0 then
      smelt.ui.notify_error("no saved sessions")
      return
    end

    local current_cwd   = smelt.session.cwd()
    local now_ms        = os.time() * 1000
    local workspace_only = true
    local query         = ""
    local filtered      = filter_entries(entries, query, workspace_only, current_cwd)

    -- List: passive display, non-focusable; selection shown via cursor_line_highlight.
    local list_buf = smelt.buf.create()
    refresh_list(list_buf, filtered, now_ms)
    local list_leaf = smelt.ui.dialog.list(list_buf, { focusable = false })

    -- Input: focused, receives typing. Filter loop wired below.
    local input_leaf = smelt.ui.dialog.input("filter sessions…")

    smelt.win.on_event(input_leaf, "text_changed", function(ctx)
      query = ctx.text or ""
      filtered = filter_entries(entries, query, workspace_only, current_cwd)
      refresh_list(list_buf, filtered, now_ms)
      smelt.win.set_cursor_row(list_leaf, 0)
    end)

    local function nav(delta)
      return function() smelt.win.move_cursor(list_leaf, delta) end
    end

    local picked = smelt.ui.dialog.open({
      title  = "resume",
      height = 70,
      panels = {
        { leaf = input_leaf, height = 1      },
        { leaf = list_leaf,  height = "fill" },
      },
      focus  = input_leaf,
      keymaps = {
        { key = "up",     on_press = nav(-1)  },
        { key = "down",   on_press = nav(1)   },
        { key = "ctrl-k", on_press = nav(-1)  },
        { key = "ctrl-j", on_press = nav(1)   },
        { key = "pgup",   on_press = nav(-10) },
        { key = "pgdn",   on_press = nav(10)  },
        { key = "alt-w", hint = "⌥w: toggle workspace filter", on_press = function()
            workspace_only = not workspace_only
            filtered = filter_entries(entries, query, workspace_only, current_cwd)
            refresh_list(list_buf, filtered, now_ms)
            smelt.win.set_cursor_row(list_leaf, 0)
          end },
        { key = "ctrl-d", hint = "^d: delete", on_press = function()
            local idx = (smelt.win.cursor_row(list_leaf) or 0) + 1
            local e = filtered[idx]
            if not e then return end
            smelt.session.delete(e.id)
            for i, x in ipairs(entries) do
              if x.id == e.id then table.remove(entries, i); break end
            end
            filtered = filter_entries(entries, query, workspace_only, current_cwd)
            refresh_list(list_buf, filtered, now_ms)
            smelt.win.set_cursor_row(list_leaf, 0)
          end },
      },
      on_submit = function(ctx)
        local idx = (smelt.win.cursor_row(list_leaf) or 0) + 1
        ctx.resolve(filtered[idx])
      end,
    })

    if picked then smelt.session.load(picked.id) end
  end)
end, { desc = "resume saved session", while_busy = false, startup_ok = true })
