-- Opt in with require("smelt.plugins.subagents") in init.lua.
local M = smelt.plugin("subagents")

function M.setup(opts)
  smelt.agent.enable_forks(opts)
end
M.setup()
smelt.agent.add_system_prompt([[
Use spawn_agent to delegate an independent task, or swarm to ask multiple agents
to independently solve the same task. Children inherit your context, tools,
workspace and permission limits. They do not share subsequent messages. Concurrent
writes affect the same checkout: partition file ownership or delegate read-only
analysis. Continue independent work after spawning, then call wait_agents once
when you need the results. It stays pending until all selected agents finish;
there is no polling or timeout. Review their final reports before relying on
them; a child report is not verification. Children cannot create further agents.
]])

local role = [[You are a subagent. The preceding conversation is inherited context,
not a new request to repeat the parent's work. Complete only the task below. Your
final message is the report delivered to the parent: include your findings,
changes, verification and any blockers there. You cannot spawn agents or swarms. Do not
change global configuration, switch sessions or workspaces, or ask the user
questions. If an operation needs approval, report that requirement to the parent.
Other agents may work in the same checkout; do not overwrite their changes.

Task:
]]

local permissions = { normal = "allow", plan = "allow", apply = "allow" }
local function spawn(args, count)
  return smelt.json.encode(smelt.agent.fork(role .. args.prompt, count, args.prompt))
end

smelt.tools.register({
  name = "spawn_agent",
  description = "Start an independent subagent with the current context and tools. Returns a run ID immediately; use wait_agents for results.",
  permission_defaults = permissions,
  effect = "process",
  parameters = {
    type = "object",
    properties = { prompt = { type = "string", description = "The child's specific task." } },
    required = { "prompt" },
  },
  summary = function(args) return args.prompt or "" end,
  execute = function(args) return spawn(args, 1) end,
})

smelt.tools.register({
  name = "swarm",
  description = "Start n independent subagents with one identical prompt and one shared parent-context snapshot. Concurrency defaults to 16 and is user-configurable; queued members retain the original snapshot.",
  permission_defaults = permissions,
  effect = "process",
  parameters = {
    type = "object",
    properties = {
      prompt = { type = "string", description = "The same task for every member." },
      n = { type = "integer", minimum = 1, maximum = 16, description = "Number of independent agents." },
    },
    required = { "prompt", "n" },
  },
  summary = function(args) return tostring(args.n or "?") .. " agents: " .. (args.prompt or "") end,
  execute = function(args) return spawn(args, args.n) end,
})

local transcript_defaults = require("smelt.transcript.defaults")
smelt.transcript.register_tool("wait_agents", {
  cache_key = "smelt.tool-presentation.wait_agents:v1",
  body = function(block, ctx, opts)
    local output = block.output
    if not output then return nil end
    local fields = output.content_fields
    return transcript_defaults.render_tool_output_tail((fields and fields.results) or output, ctx, opts)
  end,
})

smelt.tools.register({
  name = "wait_agents",
  description = "Wait once until every selected agent finishes, fails, or is cancelled. Stays pending with no timeout; do not poll. Returns only each agent's id, terminal status and final report, or an error/reason for failed or cancelled agents. No progress messages or transcripts. Continue independent work before calling this tool. Cancelling the wait does not stop agents; use stop_agent to cancel them.",
  permission_defaults = permissions,
  effect = "read",
  elapsed_visible = true,
  watchdog_timeout_ms = 0,
  watchdog_timeout_arg = "",
  parameters = {
    type = "object",
    properties = {
      ids = { type = "array", items = { type = "integer" }, minItems = 1, maxItems = 64 },
    },
    required = { "ids" },
  },
  summary = function(args)
    local ids = {}
    for _, id in ipairs(args.ids or {}) do ids[#ids + 1] = "#" .. tostring(id) end
    return table.concat(ids, ", ")
  end,
  execute = function(args, ctx)
    local task_id = smelt.task.alloc()
    __smelt_internal.agent.__start_wait(task_id, ctx.session_id, args.ids)
    local result = smelt.task.wait(task_id)
    if result.error then return { content = result.error, is_error = true } end
    local reports, previews = {}, {}
    for _, run in ipairs(result.runs) do
      local report = { id = run.id, status = run.status }
      if run.status == "completed" then
        report.result = run.result or ""
      else
        report.error = run.error or (run.status == "cancelled" and "subagent was cancelled" or "subagent failed")
      end
      reports[#reports + 1] = report
      previews[#previews + 1] = "agent #" .. run.id .. " - " .. run.status .. "\n" .. (report.result or report.error)
    end
    return { content = smelt.json.encode(reports), display_content = { results = table.concat(previews, "\n\n") } }
  end,
})

smelt.tools.register({
  name = "stop_agent",
  description = "Cancel one queued or running subagent. Other agents and the parent continue; the cancelled transcript remains available.",
  permission_defaults = permissions,
  effect = "process",
  parameters = { type = "object", properties = { id = { type = "integer" } }, required = { "id" } },
  execute = function(args) smelt.agent.stop(args.id); return "Cancellation requested." end,
})

local active

function M.open()
  if active then active() end
  local parent_id = smelt.session.info().id
  local layout = smelt.ui.layout
  local rows, timer, overlay, layout_key = {}, nil, nil, nil
  local closed, detail = false, false
  local focused = "runs"
  local sidebar_count
  local counts, totals = "no subagents", "0 tokens"
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
    render = function(row)
      if row.header then return { text = "Swarm " .. row.group .. ": " .. row.header } end
      local run = row.run
      local task = row.grouped and "" or "  " .. run.task:gsub("%s+", " ")
      local cost = run.cost_usd > 0 and string.format("  $%.4f", run.cost_usd) or ""
      local text = string.format("  #%d %-9s%s%s", run.id, run.status, cost, task)
      local hl = (run.status == "queued" or run.status == "running") and "SmeltToolPending"
        or (run.status == "failed" and "ErrorMsg")
        or (run.status == "cancelled" and "Comment") or nil
      return { text = text, marks = hl and { { col = 0, opts = { end_col = #text, hl_group = hl } } } }
    end,
  })
  local function selected()
    local row = list:selected()
    if row and row.header then
      list:move_cursor(1)
      row = list:selected()
    end
    return row and row.run
  end
  local function narrow() return (smelt.ui.size().width or 80) < 90 end
  local opts = {
    name = "smelt.subagents", anchor = "center", width = "94%", height = "85%",
    modal = true, blocks_agent = false, border = "none",
  }
  local function draw()
    local run = selected()
    local title = run and string.format(" agent %d - %s - %s ", run.id, run.status,
      run.task:gsub("%s+", " ")) or " transcript "
    local compact = narrow()
    status_buf:lines(compact and { counts, totals } or { counts .. "   " .. totals })
    local key = title .. tostring(compact) .. tostring(detail)
    if key == layout_key then return end
    layout_key = key
    local left = layout.leaf(sidebar, {
      border = compact and { all = "Comment" } or { top = "Comment", bottom = "Comment", left = "Comment" },
      title = smelt.dialog.title(" agents "),
    })
    local right = layout.leaf(preview, {
      border = (compact or detail) and { all = "Comment" } or { top = "Comment", bottom = "Comment", right = "Comment" },
      title = smelt.dialog.title(title),
    })
    local body = detail and right or (compact and left or split:layout(left, right))
    opts.layout = layout.vbox({
      { layout.leaf(status, { border = { all = "Comment" }, title = smelt.dialog.title(" subagents ") }), height = compact and 4 or 3 },
      { body, height = "fill" },
    })
    overlay = smelt.overlay.new(opts)
    if narrow() and not detail then focused = "runs"; sidebar:focus() end
  end
  local function render_preview()
    if closed or (narrow() and not detail) then return end
    local run = selected()
    if not run then return end
    local rect = preview:rect()
    if not rect or rect.height < 1 then return end
    local ok, result = pcall(smelt.session.render_preview_into, run.session_id, {
      buf = preview_buf, win = preview, width = preview:content_width() or 80, height = rect.height,
    })
    if not ok then
      preview_buf:lines({ "Transcript unavailable:", tostring(result) })
    elseif not result or result.status == "pending" then
      preview_buf:lines({ "Loading transcript..." })
    elseif result.status == "ready" and result.total_rows == 0 then
      preview_buf:lines({ run.status == "queued" and "Queued - waiting for an execution slot."
        or "Waiting for the first response..." })
    end
    -- The native preview installs its own diagnostic for unavailable storage.
  end
  local function refresh()
    if closed then return end
    local runs = smelt.agent.runs(parent_id)
    local groups, running, queued, finished = {}, 0, 0, 0
    local cost, tokens = 0, 0
    for _, run in ipairs(runs) do
      groups[run.group] = (groups[run.group] or 0) + 1
      if run.status == "running" then running = running + 1
      elseif run.status == "queued" then queued = queued + 1
      else finished = finished + 1 end
      cost = cost + run.cost_usd
      local usage = run.usage or {}
      -- Reasoning is already included in completion tokens; context is not cumulative.
      tokens = tokens + (usage.prompt_tokens or 0) + (usage.completion_tokens or 0)
        + (usage.cache_read_tokens or 0) + (usage.cache_write_tokens or 0)
    end
    rows = {}
    local previous_group
    for _, run in ipairs(runs) do
      if groups[run.group] > 1 and previous_group ~= run.group then
        rows[#rows + 1] = { key = "group:" .. run.group, group = run.group, header = run.task:gsub("%s+", " ") }
      end
      rows[#rows + 1] = { key = "agent:" .. run.id, run = run, grouped = groups[run.group] > 1 }
      previous_group = run.group
    end
    counts = #runs == 0 and "no subagents" or string.format("%d running / %d queued / %d done", running, queued, finished)
    totals = smelt.text.format_tokens(tokens) .. " tokens" .. (cost > 0 and string.format("   $%.4f", cost) or "")
    list:set_items_preserve(rows, function(row) return row.key end)
    draw()
    render_preview()
  end
  local function close()
    if closed then return end
    closed = true
    if timer then timer:remove(); timer = nil end
    if overlay then overlay:close() end
    active = nil
  end
  local function focus(pane)
    focused, sidebar_count = pane, nil
    if narrow() then detail = pane == "preview" end
    draw()
    if pane == "preview" then preview:focus() else sidebar:focus() end
    render_preview()
  end
  local function nav(delta)
    local index = list:selected_index()
    if not index then return end
    local step = delta < 0 and -1 or 1
    for _ = 1, math.min(math.abs(delta), #rows) do
      local next_index = index + step
      while rows[next_index + 1] and rows[next_index + 1].header do next_index = next_index + step end
      if not rows[next_index + 1] then break end
      index = next_index
    end
    list:set_cursor(index)
    draw()
    render_preview()
  end
  opts.keymaps = {
    { key = "esc", on_press = function()
      if detail then detail = false; focus("runs") else close() end
    end },
    { key = "ctrl-c", on_press = close },
    { key = "tab", on_press = function()
      detail = false
      focus(focused == "runs" and "preview" or "runs")
    end },
    { key = "enter", on_press = function() detail = true; focus("preview") end },
    { key = "alt-s", on_press = function()
      local run = selected()
      if run then smelt.agent.stop(run.id); refresh() end
    end },
  }
  local function sidebar_key(key, action)
    sidebar:key(key, function()
      local count = sidebar_count or 1
      sidebar_count = nil
      action(count)
    end)
  end
  for digit = 0, 9 do
    sidebar:key(tostring(digit), function()
      if sidebar_count or digit > 0 then
        sidebar_count = math.min((sidebar_count or 0) * 10 + digit, #rows)
      end
    end)
  end
  for _, binding in ipairs({
    { "j", 1 }, { "k", -1 }, { "down", 1 }, { "up", -1 },
    { "ctrl-j", 1 }, { "ctrl-k", -1 }, { "ctrl-n", 1 }, { "ctrl-p", -1 },
  }) do
    sidebar_key(binding[1], function(count) nav(binding[2] * count) end)
  end
  for _, key in ipairs({ "g", "home" }) do
    sidebar_key(key, function() nav(-#rows) end)
  end
  for _, key in ipairs({ "G", "end" }) do
    sidebar_key(key, function() nav(#rows) end)
  end
  for _, binding in ipairs({
    { "ctrl-u", -0.5 }, { "ctrl-d", 0.5 },
    { "ctrl-b", -1 }, { "ctrl-f", 1 }, { "pgup", -1 }, { "pgdn", 1 },
  }) do
    sidebar_key(binding[1], function(count)
      local height = (sidebar:rect() or {}).height or 10
      local delta = math.max(1, math.floor(height * math.abs(binding[2]))) * count
      nav(binding[2] < 0 and -delta or delta)
    end)
  end
  sidebar:on("focus", function() focused, sidebar_count = "runs", nil end)
  preview:on("focus", function() focused, sidebar_count = "preview", nil end)
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
  smelt.cmd.register("subagents", M.open, { desc = "Inspect subagents and their live read-only transcripts", busy = "run" })
end

return M
