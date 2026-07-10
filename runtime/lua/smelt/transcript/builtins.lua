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
  if child.status == "pending" then return "SmeltToolPending" end
  if child.status == "confirm" then return "SmeltAccent" end
  return "SmeltSuccess"
end

local function aggregate_status_hl(group)
  local has_pending = false
  local has_confirm = false
  for _, child in ipairs(defaults.group_children(group)) do
    if defaults.child_failed(child) then return "ErrorMsg" end
    has_pending = has_pending or child.status == "pending"
    has_confirm = has_confirm or child.status == "confirm"
  end
  if has_pending then return "SmeltToolPending" end
  if has_confirm then return "SmeltAccent" end
  return "SmeltSuccess"
end

local function summary_line(text, has_failure)
  local span = { text = text }
  if has_failure then span.hl = "ErrorMsg" end
  return layout.runs({ { span } })
end

local function tool_group_header(name, count, hl, suffix)
  return layout.runs({ {
    { text = "*", hl = hl },
    { text = " " .. name, dim = true },
    { text = " ×" .. tostring(count), dim = true, selectable = false },
    { text = suffix or "", hl = suffix ~= "" and "ErrorMsg" or nil, selectable = false },
  } })
end

local function display_path(path)
  if type(path) ~= "string" or path == "" then return nil end
  if smelt.path and smelt.path.display then return smelt.path.display(path) end
  return path
end

local function read_file_label(child)
  local args = child.args or {}
  local content = child.output and child.output.content
  if smelt.tools and smelt.tools.read_file_summary and args.file_path then
    return smelt.tools.read_file_summary(args, content)
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

local function explore_label(child)
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
    local span = { text = tostring(label(child)) }
    local hl = child_status_hl(child)
    if hl == "ErrorMsg" then span.hl = hl end
    lines[#lines + 1] = { span }
  end
  if #lines == 0 then return layout.empty() end
  return layout.gutter(layout.runs(lines), { text = "  " })
end

local function render_terminal_tool_group(group, ctx, opts)
  local count = group.child_count or #defaults.group_children(group)
  local errors, denied = defaults.group_failure_counts(group)
  local header = tool_group_header(opts.name, count, aggregate_status_hl(group), failure_suffix(errors, denied))
  if group.view_state == "expanded" then
    return defaults.render_group_children(group, ctx)
  end
  return layout.vbox({
    header,
    render_compact_group_list(group, opts.label),
  })
end

local function process_exit_code(child)
  local code = child.exit_code
  if code == nil and type(child.event_data) == "table" then code = child.event_data.exit_code end
  return code
end

local function process_id(child)
  local id = child.process_id
  if id == nil and type(child.event_data) == "table" then id = child.event_data.process_id end
  return id
end

local function process_failed(child)
  local code = tonumber(process_exit_code(child))
  return code ~= nil and code ~= 0
end

local function process_status_fragment(child)
  local id = process_id(child)
  local subject = id and tostring(id) or "process"
  local code = tonumber(process_exit_code(child))
  if code == 0 then return subject .. " finished successfully" end
  if code ~= nil then return subject .. " exited with code " .. tostring(code) end
  return subject .. " exited"
end

local function failed_process_summary(failed)
  if #failed == 0 then return "" end
  if #failed == 1 then return ": " .. process_status_fragment(failed[1]) end

  local max = math.min(#failed, 2)
  local parts = {}
  for i = 1, max do
    parts[#parts + 1] = tostring(process_id(failed[i]) or process_status_fragment(failed[i]))
  end
  if #failed > max then parts[#parts + 1] = "+" .. tostring(#failed - max) .. " more" end
  return ": " .. table.concat(parts, ", ")
end

local function render_background_process_completed_group(group, ctx)
  if group.view_state == "expanded" then return defaults.render_group_children(group, ctx) end

  local children = defaults.group_children(group)
  local count = group.child_count or #children
  local failed = {}
  for _, child in ipairs(children) do
    if process_failed(child) then failed[#failed + 1] = child end
  end

  local text = "background processes finished: " .. tostring(count)
  if #failed > 0 then
    text = text .. ", " .. tostring(#failed) .. " failed" .. failed_process_summary(failed)
  end
  return summary_line(text, #failed > 0)
end

function M.register()
  smelt.transcript.groups.register({
    name = "background_process_completed",
    cache_key = "smelt.transcript.group.background_process_completed:v1",
    priority = BUILTIN_GROUP_PRIORITY,
    min = 2,
    default_view = "collapsed",
    selector = { kind = "process_status", event = "background_process_completed" },
    render = render_background_process_completed_group,
  })

  smelt.transcript.groups.register({
    name = "explore",
    cache_key = "smelt.transcript.group.explore:v1",
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
        "inspect_symbol",
        "inspect_symbol_at",
        "find_definition",
        "find_references",
      },
    },
    render = function(group, ctx)
      return render_terminal_tool_group(group, ctx, {
        name = "explore",
        label = explore_label,
      })
    end,
  })
end

package.loaded["smelt.transcript.builtins"] = M

return M
