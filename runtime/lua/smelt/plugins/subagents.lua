-- Opt in with require("smelt.plugins.subagents") in init.lua.
local M = smelt.plugin("subagents")

local swarm_registration
local swarm_tool
function M.setup(opts)
  opts = opts or {}
  smelt.agent.enable_forks(opts)
  if swarm_registration then swarm_registration:remove(); swarm_registration = nil end
  if opts.swarm then swarm_registration = smelt.tools.register(swarm_tool) end
end
smelt.agent.add_system_prompt([[
Subagents inherit a snapshot of your context, tools, workspace and permission
limits, not subsequent messages. Concurrent writes affect the same checkout:
partition file ownership or delegate read-only analysis. After assigning parallel
work, continue your own work and collect the selected reports with wait_agents
when needed. It waits for terminal results without polling. peek_agent is for
occasional diagnosis. follow_up_agent continues a finished worker with its own
context. Review reports before relying on them; a report is not verification.
Children cannot create further agents. stop_agents cancels the whole delegated
batch without interrupting your own work.
]])

local role = [[You are a subagent. The preceding conversation is inherited context,
not a new request to repeat the parent's work. Complete only the task below. Your
final message is the report delivered to the parent: include your findings,
changes, verification and any blockers there. Use report_agent to explicitly record
whether your assignment is completed or blocked, including the report, then end
your response. A denied operation is a blocker unless you can complete the task
another way. You cannot spawn agents or swarms. Do not
change global configuration, switch sessions or workspaces, or ask the user
questions. If an operation needs approval, report that requirement to the parent.
Other agents may work in the same checkout; do not overwrite their changes.

Task:
]]

local permissions = { normal = "allow", plan = "allow", apply = "allow" }
local transcript_defaults = require("smelt.transcript.defaults")
local agent_target = { oneOf = { { type = "integer", minimum = 1 }, { type = "string", minLength = 1 } }, description = "Readable agent name or numeric run ID." }
local function handle(run)
  return { id = run.id, name = run.name, title = run.task, status = run.status }
end
local function card_handle(run)
  local info = handle(run)
  info.session_id = run.session_id
  return info
end
local function ready(ctx)
  smelt.agent.restore(ctx.session_id)
end
local function spawn(args, count)
  local runs = smelt.agent.fork(role .. args.prompt, count, args.title or args.prompt)
  local handles, metadata = {}, {}
  for _, run in ipairs(runs) do
    handles[#handles + 1] = handle(run)
    metadata[#metadata + 1] = card_handle(run)
  end
  return { content = smelt.json.encode(handles), metadata = { agents = metadata } }
end
local function spawn_summary(args) return args and (args.title or args.prompt) or "" end
local function agent_label(id) return type(id) == "number" and ("#" .. id) or tostring(id) end
local function agent_summary(args) return args and args.id and agent_label(args.id) or "" end
local function wait_summary(args)
  local ids = args and args.ids
  if type(ids) ~= "table" then return "" end
  local labels = {}
  for _, id in ipairs(ids) do labels[#labels + 1] = agent_label(id) end
  return table.concat(labels, ", ")
end

local function cards(block)
  local output = block.output
  if not output or output.is_error then return nil end
  local agents = output.metadata and output.metadata.agents
  if type(agents) ~= "table" then return nil end
  local items, live = {}, false
  for _, saved in ipairs(agents) do
    local run = type(saved.session_id) == "string" and saved.session_id ~= ""
      and __smelt_internal.agent.__card(saved.session_id) or nil
    local status = run and run.status or "archived"
    local pending = status == "running" or status == "queued"
    live = live or pending or (run and run.archive_pending)
    local elapsed = run and string.format("  %.1fs", (run.elapsed_ms or 0) / 1000) or ""
    local cost = run and run.cost_usd > 0 and string.format("  $%.4f", run.cost_usd) or ""
    local hl = pending and "SmeltToolPending" or ((status == "failed" or status == "blocked") and "ErrorMsg") or "Comment"
    items[#items + 1] = smelt.layout.runs({
      { { text = saved.name or ("#" .. saved.id), bold = true }, { text = "  " .. status .. elapsed .. cost, hl = hl } },
      { { text = saved.title or "", dim = true } },
      pending and { { text = run.activity or "", dim = true } }
        or (run and run.persistence_error and { { text = "Archive failed: " .. run.persistence_error, hl = "ErrorMsg" } }) or nil,
    })
  end
  local node = smelt.layout.vbox(items)
  if live then node = smelt.layout.refresh(node, { after_ms = 250 }) end
  return node
end

local function agent_body(block, ctx, opts)
  if block.output and block.output.is_error then
    return transcript_defaults.render_tool_output_tail(block.output, ctx, opts)
  end
  return cards(block)
end

for _, tool in ipairs({ "spawn_agent", "swarm", "follow_up_agent" }) do
  smelt.transcript.register_tool(tool, {
    cache_key = "smelt.tool-presentation." .. tool .. ":v3",
    compact = agent_body,
    title = function(block)
      return tool == "follow_up_agent" and agent_summary(block.args) or spawn_summary(block.args)
    end,
    body = agent_body,
  })
end

smelt.tools.register({
  name = "spawn_agent",
  description = "Delegate a substantial, self-contained task that can make progress in parallel with other useful work, or a task the user explicitly asks to assign to a subagent. Specify the scope and expected report. Returns a run ID and readable name immediately. Continue independent work, then use wait_agents when you need the result.",
  permission_defaults = permissions,
  effect = "process",
  parameters = {
    type = "object",
    properties = {
      title = { type = "string", maxLength = 80, description = "Short task label, such as Audit permissions." },
      prompt = { type = "string", description = "The child's specific scope, expected deliverable, and file ownership when editing." },
    },
    required = { "title", "prompt" },
  },
  summary = spawn_summary,
  execute = function(args) return spawn(args, 1) end,
})

swarm_tool = {
  name = "swarm",
  description = "Run multiple independent reviews or explorations of the same task when the user explicitly requests redundant agents. All members inherit the same snapshot and task; this does not guarantee diverse findings. They share the checkout, so assign analysis rather than overlapping edits. Use spawn_agent for distinct parallel tasks.",
  permission_defaults = permissions,
  effect = "process",
  parameters = {
    type = "object",
    properties = {
      title = { type = "string", maxLength = 80, description = "Short label for the independent reviews." },
      prompt = { type = "string", description = "The same analysis task for every member, including the expected report." },
      n = { type = "integer", minimum = 1, maximum = 16, description = "Number of independent agents." },
    },
    required = { "title", "prompt", "n" },
  },
  summary = function(args) return tostring(args.n or "?") .. " agents: " .. spawn_summary(args) end,
  execute = function(args) return spawn(args, args.n) end,
}

smelt.transcript.register_tool("peek_agent", {
  cache_key = "smelt.tool-presentation.peek_agent:v1",
  title = function(block) return agent_summary(block.args) end,
})
smelt.tools.register({
  name = "peek_agent",
  description = "Read a bounded snapshot of one subagent's assistant output and current status without waiting or consuming it, like read_process_output. Includes child-written messages and any in-flight text, not inherited context, reasoning or tool results. Running output is partial, not a final report. Use only for an occasional progress check or diagnosis. Do not poll or call repeatedly to wait for completion; continue independent work, then use wait_agents once for final results.",
  permission_defaults = permissions,
  effect = "read",
  elapsed_visible = true,
  parameters = {
    type = "object",
    properties = { id = agent_target },
    required = { "id" },
  },
  summary = agent_summary,
  execute = function(args, ctx)
    local snapshot = __smelt_internal.agent.__peek(ctx.session_id, args.id)
    local parts = { "agent #" .. snapshot.id .. " - " .. snapshot.status }
    parts[#parts + 1] = snapshot.output ~= "" and snapshot.output or "(no assistant output)"
    local reason = snapshot.error or (snapshot.status == "cancelled" and "subagent was cancelled")
    if reason then parts[#parts + 1] = "reason: " .. reason end
    return table.concat(parts, "\n\n")
  end,
})

smelt.transcript.register_tool("wait_agents", {
  cache_key = "smelt.tool-presentation.wait_agents:v2",
  title = function(block) return wait_summary(block.args) end,
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
      ids = { type = "array", items = agent_target, minItems = 1, maxItems = 64 },
    },
    required = { "ids" },
  },
  summary = wait_summary,
  execute = function(args, ctx)
    ready(ctx)
    local task_id = smelt.task.alloc()
    __smelt_internal.agent.__start_wait(task_id, ctx.session_id, args.ids)
    local result = smelt.task.wait(task_id)
    if result.error then return { content = result.error, is_error = true } end
    local reports, previews = {}, {}
    for _, run in ipairs(result.runs) do
      local report = handle(run)
      if run.status == "completed" or run.status == "blocked" then report.result = run.result or "" end
      if run.status ~= "completed" then
        report.error = run.error or (run.status == "cancelled" and "subagent was cancelled" or run.status == "blocked" and "assignment blocked" or "subagent failed")
      end
      if run.persistence_error then report.persistence_error = run.persistence_error end
      reports[#reports + 1] = report
      previews[#previews + 1] = (run.name or ("#" .. run.id)) .. " - " .. run.status .. "\n" .. (report.result or report.error)
    end
    return { content = smelt.json.encode(reports), display_content = { results = table.concat(previews, "\n\n") } }
  end,
})

smelt.tools.register({
  name = "stop_agent",
  description = "Cancel one queued or running subagent. Other agents and the parent continue; the cancelled transcript remains available.",
  permission_defaults = permissions,
  effect = "process",
  parameters = { type = "object", properties = { id = agent_target }, required = { "id" } },
  execute = function(args, ctx) __smelt_internal.agent.__stop(ctx.session_id, args.id); return "Cancellation requested." end,
})

smelt.tools.register({
  name = "stop_agents",
  description = "Cancel every queued or running subagent assigned by this conversation without interrupting the parent. Reports and transcripts remain available.",
  permission_defaults = permissions, effect = "process",
  parameters = { type = "object", properties = {} },
  execute = function(_, ctx) smelt.agent.stop_all(ctx.session_id); return "Cancellation requested for all subagents." end,
})

smelt.tools.register({
  name = "report_agent",
  description = "Subagents only: explicitly report whether your assignment is completed or blocked. Include findings, changes, verification and blockers. A completed engine response alone does not prove the assignment is complete. After this tool, end your response.",
  permission_defaults = permissions, effect = "read",
  parameters = { type = "object", properties = {
    status = { type = "string", enum = { "completed", "blocked" } },
    report = { type = "string", minLength = 1 },
  }, required = { "status", "report" } },
  summary = function(args) return args.status end,
  execute = function(args) smelt.agent.report(args.status, args.report); return "Assignment report recorded. End your response." end,
})

smelt.tools.register({
  name = "follow_up_agent",
  description = "Continue a finished subagent with its own retained context and a specific follow-up assignment. Reuses its name and session without adding subsequent parent messages. Wait for the previous assignment before using this tool. Returns immediately after the follow-up is queued.",
  permission_defaults = permissions, effect = "process", watchdog_timeout_ms = 0,
  parameters = { type = "object", properties = {
    id = agent_target, prompt = { type = "string", minLength = 1 },
  }, required = { "id", "prompt" } },
  summary = agent_summary,
  execute = function(args, ctx)
    ready(ctx)
    local task_id = smelt.task.alloc()
    __smelt_internal.agent.__start_follow_up(task_id, ctx.session_id, args.id)
    local result = smelt.task.wait(task_id)
    if result.error then return { content = result.error, is_error = true } end
    local run = __smelt_internal.agent.__follow_up(ctx.session_id, args.id, role .. args.prompt)
    local info = handle(run)
    return { content = smelt.json.encode(info), metadata = { agents = { card_handle(run) } } }
  end,
})

local active

function M.open()
  if active then active() end
  local parent_id = smelt.session.info().id
  local layout = smelt.ui.layout
  local rows, timer, overlay, layout_key = {}, nil, nil, nil
  local closed, detail, inherited = false, false, false
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
      local elapsed = run.elapsed_ms and string.format("  %.1fs", run.elapsed_ms / 1000) or ""
      local text = string.format("  %s %-9s%s%s%s", run.name or ("#" .. run.id), run.status, elapsed, cost, task)
      local hl = (run.status == "queued" or run.status == "running") and "SmeltToolPending"
        or ((run.status == "failed" or run.status == "blocked") and "ErrorMsg")
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
    local title = run and string.format(" %s - %s - %s ", run.name or ("#" .. run.id), run.status,
      run.activity or run.task:gsub("%s+", " ")) or " transcript "
    title = title .. (inherited and " full context " or " own work ")
    local compact = narrow()
    local status_lines = compact and { counts, totals } or { counts .. "   " .. totals }
    status_lines[#status_lines + 1] = compact and "Alt-S stop  Alt-A all  Alt-I context" or "Tab panes   Enter expand   Alt-S stop selected   Alt-A stop all   Alt-I context"
    status_buf:lines(status_lines)
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
      { layout.leaf(status, { border = { all = "Comment" }, title = smelt.dialog.title(" subagents ") }), height = compact and 5 or 4 },
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
      include_inherited_context = inherited,
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
    local failed, cancelled, blocked = 0, 0, 0
    local cost, tokens = 0, 0
    for _, run in ipairs(runs) do
      groups[run.group] = (groups[run.group] or 0) + 1
      if run.status == "running" then running = running + 1
      elseif run.status == "queued" then queued = queued + 1
      elseif run.status == "failed" then failed = failed + 1
      elseif run.status == "cancelled" then cancelled = cancelled + 1
      elseif run.status == "blocked" then blocked = blocked + 1
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
    if blocked > 0 then counts = counts .. string.format(" / %d blocked", blocked) end
    if failed > 0 then counts = counts .. string.format(" / %d failed", failed) end
    if cancelled > 0 then counts = counts .. string.format(" / %d cancelled", cancelled) end
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
    { key = "alt-a", on_press = function() smelt.agent.stop_all(parent_id); refresh() end },
    { key = "alt-i", on_press = function() inherited = not inherited; layout_key = nil; draw(); render_preview() end },
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

M.setup()
return M
