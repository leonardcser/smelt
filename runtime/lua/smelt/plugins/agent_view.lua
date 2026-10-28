-- Read-only agent observation. Does not enable delegation or add model tools.
local M = smelt.plugin("agent_view")
local active

function M.open()
  if active then active() end
  local parent_id = smelt.session.info().id
  local layout = smelt.ui.layout
  local closed, detail, focused = false, false, "runs"
  local overlay, timer, layout_key
  local rows, records, rows_key = {}, {}, nil
  local function ordered(runs)
    local by_id, children, roots, result, seen = {}, {}, {}, {}, {}
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
    local function visit(run, depth)
      if seen[run.id] then return end
      seen[run.id], run.depth = true, depth
      table.insert(result, run)
      for _, child in ipairs(children[run.id] or {}) do visit(child, depth + 1) end
    end
    for _, run in ipairs(roots) do visit(run, 0) end
    for _, run in ipairs(runs) do visit(run, 0) end
    return result
  end
  local function window(name, surface)
    local buf = smelt.buf.new({ readonly = true })
    local win = smelt.win.new(buf, {
      name = "smelt.subagents." .. name, region = "subagents_overlay",
      surface = surface, vim_enabled = surface == "readonly_text",
      wrap = false, scrollbar = true, pad_left = 1, pad_right = 1,
    })
    return win, buf
  end
  local sidebar, sidebar_buf = window("runs", "list_inert")
  local preview, preview_buf = window("transcript", "readonly_text")
  local status, status_buf = window("status", "selectable_text")
  preview_buf:lines({ "Select an agent to view its transcript." })
  local split = layout.split("horizontal", { size = 34, min_first = 24, min_second = 32 })
  local list = smelt.list.new({
    leaf = sidebar, buf = sidebar_buf, items = rows, empty_text = "  (no subagents)",
    render = function(run)
      local text = string.format("%s%s  %s", string.rep("  ", math.min(run.depth or 0, 8) + 1), run.title, run.status)
      local hl = (run.status == "running" or run.status == "queued" or run.status == "waiting") and "SmeltToolPending"
        or (run.status == "failed" or run.status == "blocked") and "ErrorMsg" or nil
      return { text = text, marks = hl and { { col = 0, opts = { end_col = #text, hl_group = hl } } } }
    end,
  })
  local function narrow() return (smelt.ui.size().width or 80) < 90 end
  local opts = { name = "smelt.subagents", anchor = "center", width = "94%", height = "85%", modal = true, blocks_agent = false, border = "none" }
  local function draw()
    local run = list:selected()
    local title = run and (" " .. run.title .. " - " .. run.status .. " ") or " transcript "
    local compact = narrow()
    local key = title .. tostring(compact) .. tostring(detail)
    if key == layout_key then return end
    layout_key = key
    local left = layout.leaf(sidebar, { border = compact and { all = "Comment" } or { top = "Comment", bottom = "Comment", left = "Comment" }, title = smelt.dialog.title(" agents ") })
    local right = layout.leaf(preview, { border = (compact or detail) and { all = "Comment" } or { top = "Comment", bottom = "Comment", right = "Comment" }, title = smelt.dialog.title(title) })
    local body = detail and right or compact and left or split:layout(left, right)
    opts.layout = layout.vbox({
      { layout.leaf(status, { border = { all = "Comment" }, title = smelt.dialog.title(" subagents ") }), height = 6 },
      { body, height = "fill" },
    })
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
      status_buf:lines({ "Agent restoration failed; retrying:", tostring(runs) })
      draw()
      return
    end
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
      keys[i] = { run.id, run.title, run.status, run.depth }
    end
    records = next_records
    local next_key = smelt.json.encode(keys)
    if next_key ~= rows_key then
      rows_key = next_key
      list:set_items_preserve(rows, function(run) return run.id end)
    end
    local running, finished = 0, 0
    for _, run in ipairs(rows) do
      if run.status == "running" or run.status == "waiting" or run.status == "queued" then running = running + 1 else finished = finished + 1 end
    end
    local run = list:selected()
    local controls = run and run.capabilities.stop and "Alt-S stop selected" or "Stop unavailable"
    local owner = run and (run.owner == "claude_code" and "Claude Code" or "smelt") or "No agent selected"
    local usage = run and run.total_tokens and string.format("   %d tokens (agent only)", run.total_tokens) or ""
    local activity = run and (run.error and ("Agent error: " .. run.error)
      or run.persistence_error and ("Archive failed: " .. run.persistence_error)
      or run.activity ~= "" and run.activity or run.result) or ""
    status_buf:lines({ string.format("%d active / %d finished   %s", running, finished, controls),
      owner .. usage, (activity or ""):gsub("%s+", " "), "Tab panes   Enter expand   Esc close" })
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
  opts.keymaps = {
    { key = "esc", on_press = function() if detail then detail = false; focus("runs") else close() end end },
    { key = "ctrl-c", on_press = close },
    { key = "tab", on_press = function() detail = false; focus(focused == "runs" and "preview" or "runs") end },
    { key = "enter", on_press = function() detail = true; focus("preview") end },
    { key = "alt-s", on_press = function() local run = list:selected(); if run then smelt.agent.stop(parent_id, run.id); refresh() end end },
  }
  local count
  local function nav(delta)
    list:move_cursor(delta)
    draw()
    render_preview()
  end
  local function key(binding, action)
    sidebar:key(binding, function() local n = count or 1; count = nil; action(n) end)
  end
  for digit = 0, 9 do
    sidebar:key(tostring(digit), function() if count or digit > 0 then count = math.min((count or 0) * 10 + digit, #rows) end end)
  end
  for _, binding in ipairs({ { "j", 1 }, { "k", -1 }, { "down", 1 }, { "up", -1 } }) do
    key(binding[1], function(n) nav(binding[2] * n) end)
  end
  for _, binding in ipairs({ "g", "home" }) do key(binding, function() nav(-#rows) end) end
  for _, binding in ipairs({ "G", "end" }) do key(binding, function() nav(#rows) end) end
  for _, binding in ipairs({ { "ctrl-u", -0.5 }, { "ctrl-d", 0.5 }, { "ctrl-b", -1 }, { "ctrl-f", 1 }, { "pgup", -1 }, { "pgdn", 1 } }) do
    key(binding[1], function(n) local height = (sidebar:rect() or {}).height or 10; nav(math.floor(height * binding[2]) * n) end)
  end
  sidebar:on("selection_changed", function() draw(); render_preview() end)
  sidebar:on("resized", function() draw(); render_preview() end)
  preview:on("resized", function() draw(); render_preview() end)
  active = close
  refresh()
  opts.keymaps = nil
  sidebar:focus()
  timer = smelt.timer.every(250, refresh)
end

if smelt.frontend.is_interactive() then
  smelt.cmd.register("subagents", M.open, { desc = "Inspect agents and their live read-only transcripts", busy = "run" })
end
return M
