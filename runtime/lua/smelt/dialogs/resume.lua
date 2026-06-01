-- Built-in /resume command. Telescope-style picker: search input on top, results
-- below. Arrows + Ctrl-J/K navigate; typing into the input filters; Enter loads;
-- Alt-D deletes the highlighted session; Ctrl-W toggles the workspace filter
-- between "this workspace" (default) and "all sessions".
--
-- Matching is two-tier:
--   * Title + first-user-message: fuzzy match (`smelt.fuzzy`), instant - runs
--     against cheap meta loaded up front.
--   * Full message text: substring match against the per-session `content.txt`
--     sidecar, loaded in parallel on the first non-empty query and cached for
--     the dialog lifetime. Opening the dialog stays instant; the first
--     keystroke pays the IO cost once.

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

local function make_render(now_ms)
  return function(entry)
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
    local indent = string.rep("  ", entry.depth or 0)
    return {
      text  = meta .. indent .. display_title(entry),
      marks = { { col = 0, opts = { end_col = #meta, dim = true } } },
    }
  end
end

local function update_state_label(buf, workspace_only)
  buf:clear_ns(NS_STATE)
  local label = workspace_only and " workspace " or " all "
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
    smelt.session.load(v)
  end
end)

smelt.cmd.register("resume", function()
  smelt.spawn(function()
    local entries = smelt.session.list()
    if #entries == 0 then
      smelt.notify.error("no saved sessions")
      return
    end

    local current_cwd    = smelt.session.cwd()
    local now_ms         = os.time() * 1000
    local workspace_only = true
    local query          = ""

    -- Expand flat list into DFS-ordered tree (depth field for fork indent).
    local tree_entries = smelt.session.tree(entries)

    -- Pre-build title haystacks once. Hot path runs `smelt.fuzzy.score` against
    -- these per refilter; rebuilding per call would re-concatenate strings.
    local title_hays = {}
    for _, e in ipairs(tree_entries) do
      title_hays[e.id] = display_title(e) .. " " .. (e.subtitle or "")
    end

    -- Lowercased content blobs, lazy-loaded in parallel on the first non-empty
    -- query. Stays `nil` while the user is just toggling the workspace filter.
    local texts = nil

    local function ensure_texts()
      if texts ~= nil then return end
      local ids = {}
      for _, e in ipairs(tree_entries) do table.insert(ids, e.id) end
      local raw = smelt.session.texts(ids)
      local lowered = {}
      for k, v in pairs(raw) do lowered[k] = v:lower() end
      texts = lowered
    end

    local function make_filter()
      local q_lower = query:lower()
      return function(entry)
        if workspace_only and entry.cwd ~= current_cwd then return false end
        if query == "" then return true end
        if smelt.fuzzy.score(title_hays[entry.id] or "", query) ~= nil then
          return true
        end
        local blob = texts and texts[entry.id]
        return blob ~= nil and blob:find(q_lower, 1, true) ~= nil
      end
    end

    local picked = smelt.dialog.picker({
      title       = "resume",
      height      = "70%",
      placeholder = "filter sessions…",
      items       = tree_entries,
      render      = make_render(now_ms),
      filter      = make_filter(),
      empty_text  = "  (no matching sessions)",

      on_open = function(ctx)
        update_state_label(ctx.input_buf, workspace_only)
      end,

      on_query = function(q, ctx)
        query = q
        if query ~= "" then ensure_texts() end
        ctx.list:set_filter(make_filter())
      end,

      keymaps = {
        { key = "ctrl-w", hint = "^w: workspace ⇄ all", on_press = function(ctx)
            workspace_only = not workspace_only
            update_state_label(ctx.input_buf, workspace_only)
            ctx.list:set_filter(make_filter())
          end },
        { key = "alt-d", hint = "⌥d: delete", on_press = function(ctx)
            local e = ctx.list:selected()
            if not e then return end
            smelt.session.delete(e.id)
            for i, x in ipairs(tree_entries) do
              if x.id == e.id then table.remove(tree_entries, i); break end
            end
            title_hays[e.id] = nil
            if texts then texts[e.id] = nil end
            ctx.list:set_items(tree_entries)
          end },
      },

      on_submit = function(ctx)
        if ctx.item ~= nil then ctx.resolve(ctx.item) end
      end,
    })

    if picked then smelt.session.load(picked.id) end
  end)
end, { desc = "resume saved session", while_busy = false, startup_ok = true })
