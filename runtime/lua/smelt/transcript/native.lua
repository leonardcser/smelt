-- Transcript presentation of Claude Code's own tools that no smelt tool
-- stands in for: its agents, task list, schedules, tool search, skills, LSP,
-- MCP resources, workflows, worktrees, projects, reports and notifications.
-- Each `summary` reads the call's arguments, so the header, the permission
-- dialog and group labels say what the call does. A `title` adds what the
-- call's structured result tells, which Claude Code reports as `native`
-- metadata.

local defaults = require("smelt.transcript.defaults")
local detach = require("smelt._detach")
local layout = smelt.layout

local M = {}

--- String argument `name` of `args`, trimmed, or nil when it is absent or blank.
local function text(args, name)
  local value = (args or {})[name]
  if type(value) ~= "string" then return nil end
  value = value:gsub("^%s+", ""):gsub("%s+$", "")
  if value == "" then return nil end
  return value
end

--- String argument `name` of the call as dimmed markdown, from its complete content.
local function markdown_arg(block, name)
  for _, field in ipairs(block.argument_fields or {}) do
    if field.name == name and field.content_id then
      return layout.content(field.content_id, { format = "markdown", dim = true })
    end
  end
  local value = text(block.args, name)
  return value and layout.markdown(value, { dim = true }) or nil
end

local function dim(text) return { text = text, dim = true } end

--- A body that shows `node`, and the output too when the call failed.
local function body_with_error(block, ctx, node)
  local items = {}
  items[#items + 1] = node
  if block.output and block.output.is_error then
    items[#items + 1] = defaults.render_tool_output_tail(block.output, ctx)
  end
  if #items == 0 then return nil end
  return layout.vbox(items)
end

--- A body for calls whose result only confirms them: nothing, unless it failed.
local function errors_only(block, ctx) return body_with_error(block, ctx, nil) end

--- Claude Code's structured result of a successful call, or nil.
local function result(block)
  local output = block.output
  if not output or output.is_error or type(output.metadata) ~= "table" then return nil end
  local native = output.metadata.native
  return type(native) == "table" and native or nil
end

--- A title of the call's summary followed by the spans `extra(result, args)`
--- reads from its structured result. Nil while there is neither.
local function with_result(summary, extra)
  return function(block)
    local lines = summary and summary(block.args) or nil
    if type(lines) == "string" then lines = { { { text = lines } } } end
    local data = result(block)
    local spans = data and extra(data, block.args) or {}
    if #spans == 0 then return lines end
    lines = lines or { {} }
    for _, span in ipairs(spans) do
      table.insert(lines[1], span)
    end
    return lines
  end
end

--- A body of the rows `rows_of(result)` reads from the call's structured
--- result, or of its output when the result gives none.
local function result_body(rows_of)
  return function(block, ctx)
    local data = result(block)
    local rows = data and rows_of(data) or {}
    if #rows > 0 then return layout.runs(rows) end
    return block.output and defaults.render_tool_output_tail(block.output, ctx) or nil
  end
end

--- `n` and the noun that counts it, such as `1 task` or `3 tasks`.
local function count(n, noun) return tostring(n) .. " " .. noun .. (n == 1 and "" or "s") end

--- The first line of `value` when it is a string, or nil.
local function first_line(value)
  return type(value) == "string" and value:match("^%s*([^\n]*)") or nil
end

--- A body that shows string argument `name` as markdown.
local function markdown_body(name)
  return function(block, ctx) return body_with_error(block, ctx, markdown_arg(block, name)) end
end

-- Task status glyphs, as the agents view shows run status.
local STATUS = {
  pending = { glyph = "○", fg = "Comment" },
  in_progress = { glyph = "●", fg = "SmeltAccent" },
  completed = { glyph = "✓", dim = true },
  deleted = { glyph = "✗", dim = true },
}

local function status_span(status)
  local marker = STATUS[status] or STATUS.pending
  return { text = marker.glyph .. " ", fg = marker.fg, dim = marker.dim }
end

-- ── agents ──────────────────────────────────────────────────────────

local function agent_summary(args)
  local spans = { { text = text(args, "description") or "agent" } }
  local kind = text(args, "subagent_type")
  if kind and kind ~= "general-purpose" then spans[#spans + 1] = dim("  " .. kind) end
  return { spans }
end

local function metadata(block)
  local output = block.output
  return output and type(output.metadata) == "table" and output.metadata or {}
end

local function agent_usage(block)
  local data = metadata(block)
  local parts = {}
  local tokens = tonumber(data.tokens)
  if tokens then parts[#parts + 1] = smelt.text.format_tokens(tokens) .. " tokens" end
  local uses = tonumber(data.tool_uses)
  if uses then parts[#parts + 1] = tostring(uses) .. (uses == 1 and " tool use" or " tool uses") end
  if #parts == 0 then return nil end
  return table.concat(parts, " · ")
end

local function agent_body(block, ctx)
  local output = block.output
  if block.status == "pending" or block.status == "confirm" or block.status == "drafting" then
    -- What the agent works on, as Claude Code reports it.
    local items = {}
    if output then items[#items + 1] = defaults.render_tool_output_tail(output, ctx, { rows = 4 }) end
    items[#items + 1] = detach.waiting_hint(block)
    if #items == 0 then return nil end
    return layout.vbox(items)
  end
  if not output then return nil end
  local report = not output.is_error and output.content_fields and output.content_fields.report
  if not report then return defaults.render_tool_output_tail(output, ctx) end
  local items = {
    layout.cap(layout.content(report.content_id, { format = "markdown" }), {
      rows = (ctx and ctx.limits and ctx.limits.tool_output_rows) or 20,
      keep = "head",
      marker = "below",
    }),
  }
  local usage = agent_usage(block)
  if usage then items[#items + 1] = layout.runs({ { dim(usage) } }) end
  return layout.vbox(items)
end

local function agent_compact(block)
  if block.status == "pending" then return detach.waiting_hint(block) end
  if block.status ~= "ok" then return nil end
  return agent_usage(block)
end

local function message_summary(args)
  local spans = { { text = "→ ", fg = "SmeltAgent" }, { text = text(args, "to") or "agent", bold = true } }
  local summary = text(args, "summary")
  if summary then spans[#spans + 1] = dim("  " .. summary) end
  return { spans }
end

local function message_body(block, ctx)
  local message = text(block.args, "message")
  return body_with_error(block, ctx, message and layout.markdown(message, {}) or nil)
end

-- ── task list ───────────────────────────────────────────────────────

local function todos(args)
  local list = (args or {}).todos
  return type(list) == "table" and list or {}
end

local function todo_summary(args)
  local list, done, current = todos(args), 0, nil
  for _, todo in ipairs(list) do
    if todo.status == "completed" then done = done + 1 end
    if todo.status == "in_progress" and not current then current = todo.activeForm or todo.content end
  end
  local spans = { { text = string.format("%d of %d done", done, #list) } }
  if current then spans[#spans + 1] = dim("  " .. current) end
  return { spans }
end

local function todo_body(block, ctx)
  local rows = {}
  for _, todo in ipairs(todos(block.args)) do
    local status = todo.status
    rows[#rows + 1] = {
      status_span(status),
      { text = tostring(todo.content or ""), bold = status == "in_progress", dim = status == "completed" },
    }
  end
  return body_with_error(block, ctx, #rows > 0 and layout.runs(rows) or nil)
end

local function task_create_summary(args)
  return { { status_span("pending"), { text = text(args, "subject") or "task" } } }
end

local task_create_title = with_result(task_create_summary, function(data)
  local id = type(data.task) == "table" and data.task.id
  return id and { dim("  #" .. tostring(id)) } or {}
end)

local function task_update_summary(args)
  local status = text(args, "status")
  local spans = {}
  if status then spans[1] = status_span(status) end
  spans[#spans + 1] = { text = "#" .. (text(args, "taskId") or "?"), bold = true }
  local subject = text(args, "subject")
  if subject then spans[#spans + 1] = { text = " " .. subject } end
  if status then spans[#spans + 1] = dim("  " .. status:gsub("_", " ")) end
  return { spans }
end

local function task_get_summary(args) return { { { text = "#" .. (text(args, "taskId") or "?"), bold = true } } } end

--- The tasks that block a task, as a dimmed span, or nil.
local function blocked_by(task)
  local ids = {}
  for _, id in ipairs(type(task.blockedBy) == "table" and task.blockedBy or {}) do
    ids[#ids + 1] = "#" .. tostring(id)
  end
  return #ids > 0 and dim("  blocked by " .. table.concat(ids, ", ")) or nil
end

local function task_row(task)
  local status = task.status
  local row = {
    status_span(status),
    dim("#" .. tostring(task.id or "?") .. " "),
    { text = tostring(task.subject or ""), bold = status == "in_progress", dim = status == "completed" },
  }
  if type(task.owner) == "string" and task.owner ~= "" then row[#row + 1] = dim("  " .. task.owner) end
  row[#row + 1] = blocked_by(task)
  return row
end

local function task_list(data) return type(data.tasks) == "table" and data.tasks or {} end

local task_list_title = with_result(nil, function(data)
  local tasks, done = task_list(data), 0
  for _, task in ipairs(tasks) do
    if task.status == "completed" then done = done + 1 end
  end
  return { { text = string.format("%d of %s done", done, count(#tasks, "task")) } }
end)

local task_list_body = result_body(function(data)
  local rows = {}
  for _, task in ipairs(task_list(data)) do
    rows[#rows + 1] = task_row(task)
  end
  return rows
end)

local function task_get_body(block, ctx)
  local data = result(block)
  local task = data and type(data.task) == "table" and data.task
  if not task then return block.output and defaults.render_tool_output_tail(block.output, ctx) or nil end
  local items = { layout.runs({ task_row(task) }) }
  if type(task.description) == "string" and task.description ~= "" then
    items[#items + 1] = layout.markdown(task.description, { dim = true })
  end
  return layout.vbox(items)
end

-- ── schedules ───────────────────────────────────────────────────────

--- `seconds` as its two largest units, such as `45s`, `1m 30s` or `2h`.
local function duration(seconds)
  seconds = math.floor(tonumber(seconds) or 0)
  local units = { { 3600, "h" }, { 60, "m" }, { 1, "s" } }
  for i, unit in ipairs(units) do
    if seconds >= unit[1] or i == #units then
      local out = tostring(seconds // unit[1]) .. unit[2]
      local next_unit = units[i + 1]
      local rest = next_unit and (seconds % unit[1]) // next_unit[1] or 0
      if rest > 0 then out = out .. " " .. tostring(rest) .. next_unit[2] end
      return out
    end
  end
end

local function wakeup_summary(args)
  if (args or {}).stop == true then return { { { text = "stop loop" } } } end
  local spans = { { text = "wake in " .. duration((args or {}).delaySeconds) } }
  local reason = text(args, "reason")
  if reason then spans[#spans + 1] = dim("  " .. reason) end
  return { spans }
end

--- A loop's sentinel prompt says nothing a reader needs; any other prompt shows.
local function wakeup_body(block, ctx)
  local prompt = text(block.args, "prompt")
  if prompt and prompt:match("^<<.*>>$") then return errors_only(block, ctx) end
  return body_with_error(block, ctx, markdown_arg(block, "prompt"))
end

--- Jobs recur unless the call says otherwise, so only a one-shot job is marked.
local function cron_create_summary(args)
  local spans = { { text = text(args, "cron") or "?", bold = true } }
  if (args or {}).recurring == false then spans[#spans + 1] = dim("  once") end
  return { spans }
end

--- The schedule Claude Code reads the cron expression as, before the
--- expression. Claude Code repeats the expression when it cannot phrase it.
local cron_create_title = with_result(nil, function(data, args)
  if type(data.humanSchedule) ~= "string" or data.humanSchedule == text(args, "cron") then return {} end
  local spans = { { text = data.humanSchedule, bold = true }, dim("  " .. (text(args, "cron") or "")) }
  if data.recurring == false then spans[#spans + 1] = dim("  once") end
  return spans
end)

local function cron_jobs(data) return type(data.jobs) == "table" and data.jobs or {} end

local cron_list_title = with_result(nil, function(data) return { { text = count(#cron_jobs(data), "job") } } end)

local cron_list_body = result_body(function(data)
  local rows = {}
  for _, job in ipairs(cron_jobs(data)) do
    -- A one-shot job's entry has no `recurring` field.
    rows[#rows + 1] = {
      { text = tostring(job.humanSchedule or job.cron or "?"), bold = true },
      dim(job.recurring == true and "  " or "  once  "),
      dim(tostring(job.id or "")),
      { text = "  " .. (first_line(job.prompt) or "") },
    }
  end
  return rows
end)

local function cron_delete_summary(args) return { { dim("cancel job "), { text = text(args, "id") or "?" } } } end

-- ── tools, skills and code intelligence ─────────────────────────────

local function tool_search_summary(args)
  local query = text(args, "query") or ""
  local names = query:match("^select:(.*)$")
  if names then return { { dim("load "), { text = (names:gsub("%s*,%s*", ", ")) } } } end
  return { { dim("search "), { text = query } } }
end

--- Requested tool names of a `select:` query, or nil for a search.
local function selected(args)
  local list = (text(args, "query") or ""):match("^select:(.*)$")
  if not list then return nil end
  local names = {}
  for name in list:gmatch("[^,%s]+") do
    names[#names + 1] = name
  end
  return names
end

--- The tools a selection loaded, which can be fewer than it asked for, or
--- the number of tools a search matched.
local function tool_search_title(block)
  local data = result(block)
  local matches = data and type(data.matches) == "table" and data.matches
  if not matches then return tool_search_summary(block.args) end
  local requested = selected(block.args)
  if not requested then
    local lines = tool_search_summary(block.args)
    table.insert(lines[1], dim("  " .. count(#matches, "match")))
    return lines
  end
  local spans = { dim("load "), { text = #matches > 0 and table.concat(matches, ", ") or "nothing" } }
  local missing = #requested - #matches
  if missing > 0 then spans[#spans + 1] = dim("  " .. missing .. " not found") end
  return { spans }
end

local function skill_summary(args)
  local spans = { { text = text(args, "skill") or "skill", bold = true } }
  local skill_args = text(args, "args")
  if skill_args then spans[#spans + 1] = dim("  " .. skill_args) end
  return { spans }
end

local LSP_OPERATIONS = {
  goToDefinition = "definition",
  findReferences = "references",
  hover = "hover",
  documentSymbol = "symbols",
  workspaceSymbol = "workspace symbols",
  goToImplementation = "implementations",
  prepareCallHierarchy = "call hierarchy",
  incomingCalls = "incoming calls",
  outgoingCalls = "outgoing calls",
}

local function lsp_summary(args)
  args = args or {}
  local operation = text(args, "operation")
  local spans = { { text = LSP_OPERATIONS[operation] or operation or "lsp", bold = true } }
  local query = text(args, "query")
  if operation == "workspaceSymbol" and query then
    spans[#spans + 1] = { text = " " .. query }
    return { spans }
  end
  local path = text(args, "filePath")
  if path then
    local at = smelt.path.display(path)
    -- A document's symbols do not depend on the position the call names.
    if operation ~= "documentSymbol" and args.line then
      at = at .. ":" .. tostring(args.line) .. (args.character and ":" .. tostring(args.character) or "")
    end
    spans[#spans + 1] = { text = " " .. at }
  end
  return { spans }
end

-- ── background tasks, MCP resources and the session ─────────────────

--- A monitor of a websocket runs no command, so bash's summary has nothing to show.
local function monitor_summary(args)
  local ws = (args or {}).ws
  local url = type(ws) == "table" and text(ws, "url")
  if not url then return nil end
  local spans = { dim("watch "), { text = url } }
  local description = text(args, "description")
  if description then spans[#spans + 1] = dim("  " .. description) end
  return { spans }
end

--- A summary of `prefix` and the first of the ID arguments `names` the call gives.
local function task_summary(prefix, ...)
  local names = { ... }
  return function(args)
    for _, name in ipairs(names) do
      local id = text(args, name)
      if id then return { { dim(prefix), { text = id } } } end
    end
    return { { dim(prefix), { text = "task" } } }
  end
end

local function mcp_list_summary(args)
  return { { dim("resources of "), { text = text(args, "server") or "every server" } } }
end

local function mcp_read_summary(args)
  return { { { text = text(args, "server") or "?", bold = true }, { text = " " .. (text(args, "uri") or "") } } }
end

local function task_stop_body(block, ctx)
  local data = result(block)
  local command = data and first_line(data.command)
  return body_with_error(block, ctx, command and layout.runs({ { dim(command) } }) or nil)
end

local function notification_summary(args) return text(args, "message") end

--- A notification that reached no device says why.
local function notification_body(block, ctx)
  local data = result(block)
  if data and data.pushSent == false and data.localSent == false then
    return defaults.render_tool_output_tail(block.output, ctx)
  end
  return errors_only(block, ctx)
end

-- ── workflows, worktrees, projects and other sessions ───────────────

--- A workflow's name: the saved workflow the call runs, the one its script
--- declares, or its script file. Claude Code ignores `title` and `description`
--- arguments, so the script's own are shown.
local function workflow_summary(args)
  local path = text(args, "scriptPath")
  local name = text(args, "name") or (path and smelt.path.display(path))
  local script = text(args, "script")
  local description
  if script then
    name = name or script:match("name%s*:%s*['\"]([^'\"]+)['\"]")
    description = script:match("description%s*:%s*['\"]([^'\"]+)['\"]")
  end
  local spans = { { text = name or "workflow", bold = true } }
  if description then spans[#spans + 1] = dim("  " .. description) end
  return { spans }
end

local function exit_worktree_summary(args)
  local remove = text(args, "action") == "remove"
  local spans = { { text = remove and "remove worktree" or "keep worktree" } }
  if remove and (args or {}).discard_changes == true then spans[#spans + 1] = dim("  discard changes") end
  return { spans }
end

--- A summary of the call's `key` argument as words, then the first of `targets` it gives.
local function action_summary(key, ...)
  local targets = { ... }
  return function(args)
    local spans = { { text = ((text(args, key) or "?"):gsub("_", " ")), bold = true } }
    for _, name in ipairs(targets) do
      local target = text(args, name)
      if target then
        if name:match("path$") then target = smelt.path.display(target) end
        spans[#spans + 1] = { text = " " .. target }
        break
      end
    end
    return { spans }
  end
end

local function send_to_project_summary(args)
  return { { { text = "→ ", fg = "SmeltAgent" }, { text = text(args, "project_name") or "project", bold = true } } }
end

local function send_to_project_body(block, ctx)
  local proposal = (block.args or {}).proposal
  local message = type(proposal) == "table" and text(proposal, "summary")
  return body_with_error(block, ctx, message and layout.markdown(message, {}) or nil)
end

local function power_shell_summary(args)
  local command = first_line(text(args, "command"))
  return command and { { dim("PS> "), { text = command } } } or nil
end

local function notebook_read_summary(args)
  local path = text(args, "notebook_path")
  return path and smelt.path.display(path) or nil
end

-- ── reports ─────────────────────────────────────────────────────────

local function findings(args)
  local list = (args or {}).findings
  return type(list) == "table" and list or {}
end

local function report_findings_summary(args)
  local spans = { { text = count(#findings(args), "finding") } }
  local level = text(args, "level")
  if level then spans[#spans + 1] = dim("  " .. level) end
  return { spans }
end

local function report_findings_body(block, ctx)
  local rows = {}
  for _, finding in ipairs(findings(block.args)) do
    local at = type(finding.file) == "string" and smelt.path.display(finding.file) or "?"
    if finding.line then at = at .. ":" .. tostring(finding.line) end
    rows[#rows + 1] = {
      { text = at, fg = "SmeltAccent" },
      { text = "  " .. (first_line(finding.short_summary) or first_line(finding.summary) or "") },
    }
  end
  return body_with_error(block, ctx, #rows > 0 and layout.runs(rows) or nil)
end

-- Presentations by native tool name. A tool without `body` shows its output,
-- and one without `summary` its arguments. A tool not listed shows both.
M.presentations = {
  -- Claude Code names the tool `Agent`; older versions call it `Task`.
  Agent = { summary = agent_summary, body = agent_body, compact = agent_compact, background_hl = "SmeltAgent" },
  Task = { summary = agent_summary, body = agent_body, compact = agent_compact, background_hl = "SmeltAgent" },
  SendMessage = { summary = message_summary, body = message_body },
  TodoWrite = { summary = todo_summary, body = todo_body },
  TaskCreate = { summary = task_create_summary, title = task_create_title, body = markdown_body("description") },
  TaskUpdate = { summary = task_update_summary, body = markdown_body("description") },
  TaskGet = { summary = task_get_summary, body = task_get_body },
  TaskList = { title = task_list_title, body = task_list_body },
  ScheduleWakeup = { summary = wakeup_summary, body = wakeup_body },
  CronCreate = { summary = cron_create_summary, title = cron_create_title, body = markdown_body("prompt") },
  CronDelete = { summary = cron_delete_summary, body = errors_only },
  CronList = { title = cron_list_title, body = cron_list_body },
  ToolSearch = { summary = tool_search_summary, title = tool_search_title, body = errors_only },
  Skill = { summary = skill_summary, body = errors_only },
  LSP = { summary = lsp_summary },
  NotebookRead = { summary = notebook_read_summary },
  Monitor = { summary = monitor_summary },
  TaskOutput = { summary = task_summary("output of ", "task_id") },
  BashOutput = { summary = task_summary("output of ", "bash_id") },
  TaskStop = { summary = task_summary("stop ", "task_id", "shell_id"), body = task_stop_body },
  KillShell = { summary = task_summary("stop ", "shell_id"), body = errors_only },
  KillBash = { summary = task_summary("stop ", "shell_id"), body = errors_only },
  PowerShell = { summary = power_shell_summary },
  ListMcpResourcesTool = { summary = mcp_list_summary },
  ReadMcpResourceTool = { summary = mcp_read_summary },
  EnterPlanMode = { body = errors_only },
  PushNotification = { summary = notification_summary, body = notification_body },
  Workflow = { summary = workflow_summary, body = errors_only },
  ExitWorktree = { summary = exit_worktree_summary },
  RemoteTrigger = { summary = action_summary("action", "trigger_id", "session_id") },
  DesignSync = { summary = action_summary("method", "path", "name", "projectId") },
  Artifact = { summary = action_summary("action", "file_path", "url") },
  SendToProject = { summary = send_to_project_summary, body = send_to_project_body },
  SendToProjectContext = { body = errors_only },
  ReportFindings = { summary = report_findings_summary, body = report_findings_body },
}

--- Register the presentations for the native tools of a provider named `provider`.
---@type fun(provider: string)
function M.register(provider)
  for name, presentation in pairs(M.presentations) do
    smelt.transcript.register_tool(provider .. "/" .. name, presentation)
  end
end

package.loaded["smelt.transcript.native"] = M

return M
