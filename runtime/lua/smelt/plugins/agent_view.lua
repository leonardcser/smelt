-- Read-only agent observation. Does not enable delegation or add model tools.
local M = smelt.plugin("agent_view")

smelt.keymap.scope("subagents", {
  desc = "/subagents viewer",
  actions = {
    back        = "Leave the transcript, or close the viewer",
    close       = "Close the viewer",
    switch_pane = "Switch between the run list and the transcript",
    open        = "Show the selected run's transcript",
    stop        = "Stop the selected run",
  },
  keys = {
    { "", "<Esc>", "back"        },
    { "", "q",     "back"        },
    { "", "<C-c>", "close"       },
    { "", "<Tab>", "switch_pane" },
    { "", "<CR>",  "open"        },
    { "", "<M-s>", "stop"        },
  },
})

smelt.keymap.scope("subagents.runs", {
  desc = "/subagents run list",
  actions = {
    down      = "Select the next run",
    up        = "Select the previous run",
    first     = "Select the first run",
    last      = "Select the last run",
    half_up   = "Move the selection up half a page",
    half_down = "Move the selection down half a page",
    page_up   = "Move the selection up a page",
    page_down = "Move the selection down a page",
  },
  keys = {
    { "", "j",          "down"      },
    { "", "k",          "up"        },
    { "", "<Down>",     "down"      },
    { "", "<Up>",       "up"        },
    { "", "g",          "first"     },
    { "", "<Home>",     "first"     },
    { "", "G",          "last"      },
    { "", "<End>",      "last"      },
    { "", "<C-u>",      "half_up"   },
    { "", "<C-d>",      "half_down" },
    { "", "<C-b>",      "page_up"   },
    { "", "<PageUp>",   "page_up"   },
    { "", "<C-f>",      "page_down" },
    { "", "<PageDown>", "page_down" },
  },
})
local active

local PROBLEM_NS = smelt.ns("smelt.subagents.problem")
local ACTIVE = { running = true, queued = true, waiting = true }
-- Status glyph and its style. `dim` rows are settled and step back.
local MARKERS = {
  running = { glyph = "●", style = "SmeltAccent" },
  queued = { glyph = "●", style = "Comment" },
  waiting = { glyph = "●", style = "Comment" },
  completed = { glyph = "✓", dim = true },
  cancelled = { glyph = "✗", dim = true },
  failed = { glyph = "✗", style = "ErrorMsg" },
  blocked = { glyph = "✗", style = "ErrorMsg" },
}

-- Arrange runs as a tree under `parent_agent_id`. Siblings with active work in
-- their subtree come first; otherwise the observed order is kept. Each run
-- gets a `tree_prefix` gutter (`├─ ` / `└─ `).
local function ordered(runs)
  local by_id, children, roots, live = {}, {}, {}, {}
  for _, run in ipairs(runs) do by_id[run.id] = run end
  for _, run in ipairs(runs) do
    local parent = run.parent_agent_id
    if parent and by_id[parent] then
      children[parent] = children[parent] or {}
      table.insert(children[parent], run)
    else
      table.insert(roots, run)
    end
  end
  -- Whether the run or any descendant is active. Marked false before the
  -- descent so a parent cycle ends instead of recursing.
  local function is_live(run)
    if live[run.id] == nil then
      live[run.id] = false
      local result = ACTIVE[run.status] or false
      for _, child in ipairs(children[run.id] or {}) do result = is_live(child) or result end
      live[run.id] = result
    end
    return live[run.id]
  end
  local function active_first(list)
    local first, rest = {}, {}
    for _, run in ipairs(list) do table.insert(is_live(run) and first or rest, run) end
    for _, run in ipairs(rest) do table.insert(first, run) end
    return first
  end
  local result, seen = {}, {}
  local function visit(run, rails, is_last, depth)
    if seen[run.id] then return end
    seen[run.id] = true
    run.tree_prefix = depth == 0 and "" or (table.concat(rails) .. (is_last and "└─ " or "├─ "))
    table.insert(result, run)
    local kids = active_first(children[run.id] or {})
    local next_rails = {}
    for i, rail in ipairs(rails) do next_rails[i] = rail end
    if depth > 0 then table.insert(next_rails, is_last and "   " or "│  ") end
    for i, child in ipairs(kids) do visit(child, next_rails, i == #kids, depth + 1) end
  end
  roots = active_first(roots)
  for i, run in ipairs(roots) do visit(run, {}, i == #roots, 0) end
  -- Runs caught in a parent cycle have no root; list them flat.
  for _, run in ipairs(runs) do visit(run, {}, true, 0) end
  return result
end

local function list_title(runs)
  local running, done = 0, 0
  for _, run in ipairs(runs) do
    if ACTIVE[run.status] then running = running + 1 else done = done + 1 end
  end
  local counts = {}
  if running > 0 then table.insert(counts, running .. " running") end
  if done > 0 then table.insert(counts, done .. " done") end
  if #counts == 0 then return " agents " end
  return " agents  " .. table.concat(counts, " · ") .. " "
end

function M.open()
  if active then active() end
  local parent_id = smelt.session.info().id
  local layout = smelt.ui.layout
  local closed, detail, focused = false, false, "runs"
  local overlay, timer, layout_key, restore_error
  local rows, records, rows_key = {}, {}, nil
  local function window(name, surface)
    local buf = smelt.buf.new({ readonly = true })
    local win = smelt.win.new(buf, {
      name = "smelt.subagents." .. name, surface = surface, vim_enabled = surface == "readonly_text",
      wrap = false, scrollbar = surface == "readonly_text", pad_left = 1, pad_right = 1,
      kind = surface == "list_inert" and "list" or nil, initial_cursor = surface == "list_inert" and 0 or nil,
      hide_cursor = surface == "list_inert",
    })
    return win, buf
  end
  local sidebar, sidebar_buf = window("runs", "list_inert")
  local preview, preview_buf = window("transcript", "readonly_text")
  local problem, problem_buf = window("problem", "selectable_text")
  preview_buf:lines({ "Select an agent to view its transcript." })
  local split = layout.split("horizontal", { key = "agent_view", size = 34, min_first = 24, min_second = 32 })
  local list = smelt.list.new({
    leaf = sidebar, buf = sidebar_buf, items = rows, empty_text = "(no subagents)",
    render = function(run)
      local marker = MARKERS[run.status] or MARKERS.queued
      local head = run.tree_prefix .. marker.glyph .. " "
      local tail = run.total_tokens and ("  " .. smelt.text.format_tokens(run.total_tokens)) or ""
      local width = sidebar:content_width()
      local title = run.title
      if width then
        title = smelt.text.fit(title, math.max(width - smelt.text.width(head) - smelt.text.width(tail), 1))
      end
      local text = head .. title .. tail
      local glyph_col = #run.tree_prefix
      local marks = {}
      if glyph_col > 0 then table.insert(marks, { col = 0, opts = { end_col = glyph_col, style = { dim = true } } }) end
      if tail ~= "" then table.insert(marks, { col = #text - #tail, opts = { end_col = #text, style = { dim = true } } }) end
      if marker.dim then
        table.insert(marks, { col = 0, opts = { end_col = #text, style = { dim = true } } })
      elseif marker.style then
        table.insert(marks, { col = glyph_col, opts = { end_col = glyph_col + #marker.glyph, style = marker.style } })
      end
      return { text = text, marks = marks }
    end,
  })
  local function narrow() return (smelt.ui.size().width or 80) < 90 end
  -- Problems the transcript cannot show. The transcript preview already ends
  -- with the agent's own error.
  local function problem_text()
    if restore_error then return "Agent restoration failed; retrying: " .. restore_error end
    local run = list:selected()
    return run and run.persistence_error and ("Archive failed: " .. run.persistence_error) or nil
  end
  local opts = { name = "smelt.subagents", anchor = "center", width = "94%", height = "85%", modal = true, blocks_agent = false, border = "none" }
  local function draw()
    local run = list:selected()
    local title = run and (" " .. run.title .. " ") or " transcript "
    local sidebar_title = list_title(rows)
    local issue = problem_text()
    local issue_line = issue and (issue:gsub("%s+", " ")) or ""
    problem_buf:lines({ issue_line })
    problem_buf:clear_ns(PROBLEM_NS)
    if issue then problem_buf:mark(PROBLEM_NS, 1, 0, { end_col = #issue_line, style = "ErrorMsg" }) end
    local compact = narrow()
    local key = table.concat({ title, sidebar_title, tostring(issue ~= nil), tostring(compact), tostring(detail) }, "\0")
    if key == layout_key then return end
    layout_key = key
    local left = layout.leaf(sidebar, { border = compact and { all = "Comment" } or { top = "Comment", bottom = "Comment", left = "Comment" }, title = smelt.dialog.title(sidebar_title) })
    local transcript = layout.leaf(preview)
    if issue then
      transcript = layout.vbox({ { layout.leaf(problem), height = 1 }, { transcript, height = "fill" } })
    end
    local right = layout.frame(transcript, { border = (compact or detail) and { all = "Comment" } or { top = "Comment", bottom = "Comment", right = "Comment" }, title = smelt.dialog.title(title) })
    opts.layout = detail and right or compact and left or split:layout(left, right)
    overlay = smelt.overlay.new(opts)
  end
  local function render_preview()
    if closed or (narrow() and not detail) then return end
    local run = list:selected()
    local rect = preview:rect()
    if not run or not rect or rect.height < 1 then return end
    local ok, result = pcall(smelt.session.render_preview_into, run.session_id, {
      buf = preview_buf, win = preview, width = preview:content_width() or 80, height = rect.height,
    })
    if not ok then preview_buf:lines({ "Transcript unavailable:", tostring(result) })
    elseif not result or result.status == "pending" then preview_buf:lines({ "Loading transcript..." })
    elseif result.status == "ready" and result.total_rows == 0 then preview_buf:lines({ "Waiting for the first response..." }) end
  end
  local function refresh()
    if closed then return end
    local ok, runs = pcall(smelt.agent.runs, parent_id)
    if not ok then
      restore_error = tostring(runs)
      draw()
      return
    end
    restore_error = nil
    local next_records, keys = {}, {}
    rows = ordered(runs)
    for i, run in ipairs(rows) do
      local previous = records[run.id]
      if previous then
        for key in pairs(previous) do previous[key] = nil end
        for key, value in pairs(run) do previous[key] = value end
        rows[i], run = previous, previous
      end
      next_records[run.id] = run
      keys[i] = { run.id, run.title, run.status, run.tree_prefix, run.total_tokens }
    end
    records = next_records
    local next_key = smelt.json.encode(keys)
    if next_key ~= rows_key then
      rows_key = next_key
      list:set_items_preserve(rows, function(run) return run.id end)
    end
    draw()
    render_preview()
  end
  local function close()
    if closed then return end
    closed = true
    if timer then timer:remove() end
    if overlay then overlay:close() end
    active = nil
  end
  local function focus(pane)
    focused = pane
    if narrow() then detail = pane == "preview" end
    draw()
    if pane == "preview" then preview:focus() else sidebar:focus() end
    render_preview()
  end
  local function back() if detail then detail = false; focus("runs") else close() end end
  opts.scope = "subagents"
  opts.actions = {
    back = back,
    close = close,
    switch_pane = function() detail = false; focus(focused == "runs" and "preview" or "runs") end,
    open = function() detail = true; focus("preview") end,
    stop = function() local run = list:selected(); if run then smelt.agent.stop(parent_id, run.id); refresh() end end,
  }
  local count
  local function nav(delta)
    list:move_cursor(delta)
    draw()
    render_preview()
  end
  -- A typed count repeats the next motion, as in Vim.
  local function counted(action)
    return function() local n = count or 1; count = nil; action(n) end
  end
  for digit = 0, 9 do
    sidebar:key(tostring(digit), function() if count or digit > 0 then count = math.min((count or 0) * 10 + digit, #rows) end end)
  end
  local function pages(fraction)
    return counted(function(n) local height = (sidebar:rect() or {}).height or 10; nav(math.floor(height * fraction) * n) end)
  end
  sidebar:scope("subagents.runs", {
    down = counted(function(n) nav(n) end),
    up = counted(function(n) nav(-n) end),
    first = counted(function() nav(-#rows) end),
    last = counted(function() nav(#rows) end),
    half_up = pages(-0.5),
    half_down = pages(0.5),
    page_up = pages(-1),
    page_down = pages(1),
  })
  sidebar:on("selection_changed", function() draw(); render_preview() end)
  sidebar:on("resized", function() draw(); render_preview() end)
  preview:on("resized", function() draw(); render_preview() end)
  active = close
  refresh()
  opts.scope, opts.actions = nil, nil
  sidebar:focus()
  timer = smelt.timer.every(250, refresh)
end

if smelt.frontend.is_interactive() then
  smelt.cmd.register("subagents", M.open, { desc = "Inspect agents and their live read-only transcripts", busy = "run" })
end
return M
