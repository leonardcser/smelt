-- `/mcp` - show MCP servers, lifecycle state, descriptions, and tool names.

local NS_DIM = smelt.ns("smelt.mcp.dim")
local NS_ERROR = smelt.ns("smelt.mcp.error")

local function status_label(status)
  local kind = status and status.kind or "unknown"
  if kind == "connected" then return "loaded" end
  if kind == "disabled" then return "not loaded" end
  if kind == "connecting" then return "loading" end
  if kind == "error" then return "error" end
  return kind
end

local function sorted_tool_names(tools)
  local names = {}
  for _, tool in ipairs(tools or {}) do
    names[#names + 1] = tool.name or tool.qualified_name or ""
  end
  table.sort(names)
  return names
end

local function mark(marks, ns, line, start_col, end_col, opts)
  if end_col <= start_col then return end
  opts = opts or {}
  opts.end_col = end_col
  marks[#marks + 1] = { ns = ns, line = line, start_col = start_col, opts = opts }
end

local function push(lines, text)
  lines[#lines + 1] = text
  return #lines
end

local function build_lines()
  local servers = smelt.mcp.list()
  if #servers == 0 then
    return { "No MCP servers registered." }, {}
  end

  local lines = {}
  local marks = {}
  for i, server in ipairs(servers) do
    local name = server.name or ""
    local status = status_label(server.status)
    local tool_count = server.tool_count or 0
    local title = string.format("%s  [%s]  %d tool%s", name, status, tool_count, tool_count == 1 and "" or "s")
    local line = push(lines, title)
    mark(marks, NS_DIM, line, #name + 2, #title, { dim = true })
    if status == "error" then
      local start_col = #name + 2
      mark(marks, NS_ERROR, line, start_col, start_col + #"[error]", { fg = "red", bold = true })
    end

    local desc = server.description or ""
    if desc ~= "" then
      local desc_line = push(lines, desc)
      mark(marks, NS_DIM, desc_line, 0, #desc, { dim = true })
    end

    local status_row = server.status or {}
    if status_row.kind == "error" and status_row.error and status_row.error ~= "" then
      local err = "error: " .. status_row.error
      local err_line = push(lines, err)
      mark(marks, NS_ERROR, err_line, 0, #err, { fg = "red" })
    end

    local names = sorted_tool_names(server.tools)
    if #names > 0 then
      local tools_line = push(lines, "tools:")
      mark(marks, NS_DIM, tools_line, 0, #"tools:", { dim = true })
      for _, tool_name in ipairs(names) do
        local tool_line = push(lines, "  - " .. tool_name)
        mark(marks, NS_DIM, tool_line, 0, #"  - ", { dim = true })
      end
    end

    if i < #servers then
      push(lines, "")
    end
  end
  return lines, marks
end

smelt.cmd.register("mcp", function()
  smelt.spawn(function()
    local lines, marks = build_lines()
    local buf = smelt.buf.new({ readonly = true })
    buf:lines(lines)
    for _, m in ipairs(marks) do
      buf:mark(m.ns, m.line, m.start_col, m.opts)
    end

    local leaf = smelt.win.new(buf, {
      region      = "mcp_overlay",
      surface     = "readonly_text",
      wrap        = true,
      vim_enabled = smelt.settings.vim and true or false,
    })

    smelt.overlay.new({
      anchor = "center",
      border = "none",
      modal  = true,
      width  = "85%",
      height = "75%",
      layout = smelt.ui.layout.leaf(leaf, {
        border = { all = "Comment" },
        title = smelt.dialog.title(" mcp "),
      }),
    })

    local task_id = smelt.task.alloc()
    local function close() leaf:close(); smelt.task.resume(task_id, nil) end
    leaf:key("q", close)
    leaf:on("dismiss", close)
    smelt.task.wait(task_id)
  end)
end, { desc = "show MCP servers and tools" })
