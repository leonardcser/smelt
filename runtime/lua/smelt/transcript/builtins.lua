-- Built-in transcript group rules and renderers.

local defaults = require("smelt.transcript.defaults")
local layout = smelt.layout

local M = {}
local BUILTIN_GROUP_PRIORITY = -100
local GROUP_LIST_MAX = 5

local function failure_suffix(errors, denied)
  local parts = {}
  if errors > 0 then
    parts[#parts + 1] = tostring(errors) .. (errors == 1 and " error" or " errors")
  end
  if denied > 0 then
    parts[#parts + 1] = tostring(denied) .. " denied"
  end
  if #parts == 0 then return "" end
  return " (" .. table.concat(parts, ", ") .. ")"
end

local function child_status_hl(child)
  if defaults.child_failed(child) then return "ErrorMsg" end
  if child.status == "pending" or child.status == "drafting" then return "SmeltToolPending" end
  return nil
end

local function aggregate_status(group)
  local has_pending = false
  local has_confirm = false
  for _, child in ipairs(defaults.group_children(group)) do
    if defaults.child_failed(child) then return "err" end
    has_pending = has_pending or child.status == "pending" or child.status == "drafting"
    has_confirm = has_confirm or child.status == "confirm"
  end
  if has_pending then return "pending" end
  if has_confirm then return "confirm" end
  return "ok"
end

local function latest_group_call_time(group)
  local latest
  for _, child in ipairs(defaults.group_children(group)) do
    local called_at_ms = tonumber(child.called_at_ms)
    if called_at_ms and (not latest or called_at_ms > latest) then latest = called_at_ms end
  end
  return latest
end

local function tool_group_header(group, ctx)
  local count = group.child_count or #defaults.group_children(group)
  local errors, denied = defaults.group_failure_counts(group)
  local suffix = failure_suffix(errors, denied)
  local header = setmetatable({
    name = group.group_kind or group.name or "group",
    status = aggregate_status(group),
    called_at_ms = latest_group_call_time(group),
    summary = { {
      { text = "×" .. tostring(count), dim = true, selectable = false },
      { text = suffix, hl = suffix ~= "" and "ErrorMsg" or nil, selectable = false },
    } },
  }, { __index = group })
  return smelt.transcript.render_part("tool_header", header, ctx)
end

local function display_path(path)
  if type(path) ~= "string" or path == "" then return nil end
  if smelt.path and smelt.path.display then return smelt.path.display(path) end
  return path
end

local function read_file_label(child)
  local args = child.args or {}
  local content_lines = child.output and child.output.content_lines
  if smelt.tools and smelt.tools.read_file_summary and args.file_path then
    return smelt.tools.read_file_summary(args, content_lines)
  end
  return display_path(args.file_path) or child.summary_text or child.name or "read_file"
end

local function grep_label(child)
  local args = child.args or {}
  local pattern = args.pattern or child.summary_text or ""
  local label = pattern ~= "" and ('"' .. tostring(pattern) .. '"') or "grep"
  if args.path and args.path ~= "" then
    label = label .. " in " .. (display_path(args.path) or tostring(args.path))
  elseif args.glob and args.glob ~= "" then
    label = label .. " glob:" .. tostring(args.glob)
  elseif args.type and args.type ~= "" then
    label = label .. " type:" .. tostring(args.type)
  end
  return label
end

local function glob_label(child)
  local args = child.args or {}
  local label = args.pattern or child.summary_text or ""
  if args.path and args.path ~= "" then label = label .. " in " .. (display_path(args.path) or tostring(args.path)) end
  return label
end

local function tool_group_label(child)
  local label
  if child.name == "read_file" then
    label = read_file_label(child)
  elseif child.name == "grep" then
    label = grep_label(child)
  elseif child.name == "glob" then
    label = glob_label(child)
  else
    label = child.summary_text or ""
  end
  local name = child.name or "tool"
  if label == "" or label == name then return name end
  return name .. " " .. label
end

local function render_compact_group_list(group, label)
  local children = defaults.group_children(group)
  local max = math.min(#children, GROUP_LIST_MAX)
  local lines = {}
  local start = math.max(1, #children - max + 1)
  if start > 1 then
    lines[#lines + 1] = { { text = "… " .. tostring(start - 1) .. " above", dim = true, selectable = false } }
  end
  for i = start, #children do
    local child = children[i]
    local span = { text = tostring(label(child)), hl = child_status_hl(child) }
    lines[#lines + 1] = { span }
  end
  if #lines == 0 then return layout.empty() end
  return layout.gutter(layout.runs(lines), { text = "  " })
end

local function render_terminal_tool_group(group, ctx)
  if group.view_state == "expanded" then
    return defaults.render_group_children(group, ctx)
  end
  return layout.vbox({
    tool_group_header(group, ctx),
    render_compact_group_list(group, tool_group_label),
  })
end

--- Up to `max` of `names`, then how many more there are.
local function name_list(names, max)
  local shown = {}
  for i = 1, math.min(#names, max) do shown[i] = names[i] end
  if #names > max then shown[#shown + 1] = "+" .. tostring(#names - max) .. " more" end
  return table.concat(shown, ", ")
end

local SUMMARY_LABELS = {
  process = "background processes finished",
  monitor = "monitor events",
  agent = "agent messages",
}

--- One-line summary of several deliveries of `run`: their count, then the
--- processes that failed or where the others came from. Returns the text and
--- its highlight group, `ErrorMsg` when one of them failed.
---@param run "process"|"monitor"|"agent"
---@param items table[] Delivery blocks or grouped children.
---@param count? integer Defaults to the number of `items`.
---@return string text
---@return string hl
function M.delivery_summary(run, items, count)
  local failed, sources, seen = {}, {}, {}
  for _, item in ipairs(items) do
    if item.failed then failed[#failed + 1] = item end
    local source = item.source
    if source and source ~= "" and not seen[source] then
      seen[source] = true
      sources[#sources + 1] = source
    end
  end
  local text = (SUMMARY_LABELS[run] or SUMMARY_LABELS.process) .. ": " .. tostring(count or #items)
  if run == "process" then
    if #failed == 1 then
      text = text .. ", 1 failed: " .. failed[1].source .. failed[1].source_after
    elseif #failed > 1 then
      local ids = {}
      for i, item in ipairs(failed) do ids[i] = item.source end
      text = text .. ", " .. tostring(#failed) .. " failed: " .. name_list(ids, 2)
    end
  elseif #sources > 0 then
    text = text .. " from " .. name_list(sources, 3)
  end
  return text, defaults.delivery_hl(run, #failed > 0)
end

-- Each group of deliveries, its run, and what it selects. A notice without
-- its structure does not join the processes it may name.
local DELIVERY_GROUPS = {
  { name = "processes", run = "process", fields = { event = "process_ended" } },
  { name = "monitors", run = "monitor", fields = { run = "monitor" } },
  { name = "agents", run = "agent", fields = { run = "agent" } },
}
local DELIVERY_GROUP_RUNS = {}
for _, entry in ipairs(DELIVERY_GROUPS) do DELIVERY_GROUP_RUNS[entry.name] = entry.run end

local function render_delivery_group(run, group, ctx)
  if group.view_state == "expanded" then return defaults.render_group_children(group, ctx) end
  local text, hl = M.delivery_summary(run, defaults.group_children(group), group.child_count)
  return layout.runs({ { { text = text, fg = hl, italic = true } } })
end

function M.render(group, ctx)
  local kind = group.group_kind or group.name
  local run = DELIVERY_GROUP_RUNS[kind]
  if run then return render_delivery_group(run, group, ctx) end
  if kind == "explore" or kind == "lsp" or kind == "web" then
    return render_terminal_tool_group(group, ctx)
  end
  if (ctx and ctx.view_state) == "expanded" or group.view_state == "expanded" then
    return defaults.render_group_children(group, ctx)
  end
  return tool_group_header(group, ctx)
end

function M.register()
  for _, entry in ipairs(DELIVERY_GROUPS) do
    smelt.transcript.groups.register({
      name = entry.name,
      priority = BUILTIN_GROUP_PRIORITY,
      min = 2,
      default_view = "collapsed",
      selector = { kind = "delivery", fields = entry.fields },
    })
  end

  smelt.transcript.groups.register({
    name = "explore",
    priority = BUILTIN_GROUP_PRIORITY,
    min = 2,
    default_view = "collapsed",
    selector = {
      kind = "tool",
      names = {
        "read_file",
        "grep",
        "glob",
        "outline",
        "find_symbol",
      },
    },
  })

  smelt.transcript.groups.register({
    name = "lsp",
    priority = BUILTIN_GROUP_PRIORITY,
    min = 2,
    default_view = "collapsed",
    selector = {
      kind = "tool",
      names = {
        "inspect_symbol",
        "inspect_symbol_at",
        "find_definition",
        "find_references",
        "diagnostics",
      },
    },
  })

  smelt.transcript.groups.register({
    name = "web",
    priority = BUILTIN_GROUP_PRIORITY,
    min = 2,
    default_view = "collapsed",
    selector = {
      kind = "tool",
      names = {
        "web_search",
        "web_fetch",
      },
    },
  })
end

package.loaded["smelt.transcript.builtins"] = M

return M
