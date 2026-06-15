smelt.transcript = smelt.transcript or {}
smelt.transcript.defaults = smelt.transcript.defaults or {}

local M = smelt.transcript.defaults
local layout = smelt.layout

local status_hl = {
  pending = "SmeltToolPending",
  ok = "SmeltSuccess",
  err = "ErrorMsg",
  denied = "ErrorMsg",
  confirm = "SmeltAccent",
}

--- Render any semantic transcript block with the bundled default policy.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render(block, ctx)
  block = block or {}
  if block.kind == "tool" then
    return M.render_tool(block, ctx)
  elseif block.kind == "user" then
    return M.render_user(block, ctx)
  elseif block.kind == "assistant" then
    return M.render_assistant(block, ctx)
  elseif block.kind == "thinking" then
    return M.render_thinking(block, ctx)
  elseif block.kind == "exec" then
    return M.render_exec(block, ctx)
  elseif block.kind == "mode" then
    return M.render_mode(block, ctx)
  elseif block.kind == "process_status" then
    return M.render_process_status(block, ctx)
  elseif block.kind == "compacted" then
    return M.render_compacted(block, ctx)
  elseif block.kind == "code" then
    return M.render_code(block, ctx)
  end
  return M.render_unknown(block, ctx)
end

--- Render a tool block using the current generic primitives and explicit item
--- construction.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_tool(block, ctx)
  local items = {}
  items[#items + 1] = M.render_tool_header(block, ctx)

  if block.user_message and block.user_message ~= "" then
    items[#items + 1] = layout.gutter(
      layout.text(block.user_message),
      { text = "  " }
    )
  end

  if block.status ~= "denied" then
    local body = M.render_tool_body(block, ctx)
    if body then items[#items + 1] = body end
  end

  return layout.vbox(items)
end

local tool_header_lines

--- Render the default one-line tool header.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts: table?): table
function smelt.transcript.defaults.render_tool_header(block, ctx, opts)
  local _ = ctx
  opts = opts or {}
  local status = block.status or "pending"
  local hl = opts.hl or opts.hl_group or block.status_hl or status_hl[status]
  local lines = tool_header_lines(block, status, hl)
  return layout.cap(layout.runs(lines), {
    rows = (ctx and ctx.limits and ctx.limits.tool_header_rows) or 20,
    keep = "head",
    marker = "below",
  })
end

local function copy_table(t)
  local out = {}
  for k, v in pairs(t) do
    if type(v) == "table" then
      out[k] = copy_table(v)
    else
      out[k] = v
    end
  end
  return out
end

local function copy_span(span)
  if type(span) == "string" then return { text = span } end
  if type(span) ~= "table" then return { text = tostring(span or "") } end
  return copy_table(span)
end

local function span_text(span)
  if type(span) == "string" then return span end
  if type(span) ~= "table" then return tostring(span or "") end
  return span.text or span[1] or ""
end

local function has_text(spans)
  for _, span in ipairs(spans) do
    if span_text(span) ~= "" then return true end
  end
  return false
end

local function summary_lines(summary)
  if summary == nil then return {} end
  if type(summary) == "string" then
    if summary == "" then return {} end
    local out = {}
    for line in (summary .. "\n"):gmatch("([^\n]*)\n") do
      out[#out + 1] = { { text = line } }
    end
    return out
  end
  if type(summary) ~= "table" then return {} end

  local out = {}
  for _, line in ipairs(summary) do
    local spans = {}
    if type(line) == "string" then
      spans[#spans + 1] = { text = line }
    elseif type(line) == "table" then
      for _, span in ipairs(line) do
        spans[#spans + 1] = copy_span(span)
      end
    end
    out[#out + 1] = spans
  end
  return out
end

function tool_header_lines(block, status, hl)
  local lines = summary_lines(block.summary)
  local suffix = {}
  local elapsed = block.elapsed_text
  if elapsed and elapsed ~= "" then
    suffix[#suffix + 1] = { text = elapsed, selectable = false, dim = true }
  end

  if lines[1] then
    local first = lines[1]
    local trailing = {}
    while #first > 0 and first[#first].title_suffix do
      local span = table.remove(first)
      local text = span_text(span):gsub("^%s+", ""):gsub("%s+$", "")
      if status == "pending" and text ~= "" then
        span.text = text
        span[1] = nil
        trailing[#trailing + 1] = span
      end
    end
    for i = #trailing, 1, -1 do
      suffix[#suffix + 1] = trailing[i]
    end
  end

  if #lines == 0 then lines[1] = {} end
  local first = lines[1]
  local has_summary = has_text(first)
  local tail = has_summary or #suffix > 0
  local prefix = {
    { text = "*", hl = hl },
    { text = " " .. (block.name or "tool"), dim = true },
  }
  if tail then prefix[#prefix + 1] = { text = " ", selectable = has_summary } end
  for i = #prefix, 1, -1 do
    table.insert(first, 1, prefix[i])
  end

  if #suffix > 0 then
    if has_summary then
      first[#first + 1] = { text = "  ", selectable = false, dim = true }
    end
    for i, span in ipairs(suffix) do
      if i > 1 then first[#first + 1] = { text = " ", selectable = false, dim = true } end
      local copy = copy_span(span)
      copy.selectable = false
      if copy.dim == nil then copy.dim = true end
      first[#first + 1] = copy
    end
  end

  return lines
end

local function edit_fields(args)
  return args.file_path or "", args.old_string or "", args.new_string or "", args.replace_all == true
end

local function replace_first(haystack, needle, replacement)
  local s, e = string.find(haystack, needle, 1, true)
  if not s then return haystack end
  return haystack:sub(1, s - 1) .. replacement .. haystack:sub(e + 1)
end

local function replace_all(haystack, needle, replacement)
  local out = {}
  local start = 1
  while true do
    local s, e = string.find(haystack, needle, start, true)
    if not s then
      out[#out + 1] = haystack:sub(start)
      break
    end
    out[#out + 1] = haystack:sub(start, s - 1)
    out[#out + 1] = replacement
    start = e + 1
  end
  return table.concat(out)
end

local function apply_edit(content, old_string, new_string, do_all)
  if do_all then return replace_all(content, old_string, new_string) end
  return replace_first(content, old_string, new_string)
end

local function planned_edit_diff(args)
  local path, old_string, new_string, do_all = edit_fields(args)
  local cached = path ~= "" and smelt.fs.file_state.get(path) or nil
  local content = cached and cached.content or nil
  if not content then
    return layout.diff({
      old = old_string,
      new = new_string,
      path = path,
      anchor = old_string,
    })
  end
  return layout.diff({
    old = content,
    new = apply_edit(content, old_string, new_string, do_all),
    path = path,
    anchor = old_string,
  })
end

local function notebook_preview_layout(meta)
  meta = meta or {}
  local lang = meta.syntax_ext
  local path = meta.path or ""
  local body
  if meta.edit_mode == "insert" then
    body = layout.file_view({
      content = meta.new_source or "",
      path = path .. "." .. (lang or "py"),
      lang = lang,
    })
  else
    body = layout.diff({
      old = meta.old_source or "",
      new = meta.new_source or "",
      path = lang and (path .. "." .. lang) or path,
      lang = lang,
    })
  end
  local title = meta.title or ""
  if title == "" then return body end
  return layout.vbox({ layout.text(title), body })
end

local tool_body_renderers = {
  bash = function(block, ctx)
    local content = ((block.output and block.output.content) or ""):gsub("%s+$", "")
    if not content:match("%S") then return nil end
    return M.render_tool_output({ content = content, is_error = block.output.is_error }, ctx)
  end,
  edit_file = function(block)
    local args = block.args or {}
    local meta = block.output and block.output.metadata
    if meta then
      return layout.diff({
        old = meta.old_content or args.old_string or "",
        new = meta.new_content or args.new_string or "",
        path = meta.path or args.file_path or "",
        anchor = args.old_string or "",
      })
    end
    return planned_edit_diff(args)
  end,
  edit_notebook = function(block)
    return notebook_preview_layout((block.output and block.output.metadata) or {})
  end,
  exit_plan_mode = function(block)
    return layout.text((block.args and block.args.plan_summary) or "")
  end,
  glob = function(block)
    return layout.text(smelt.text.line_count((block.output and block.output.content) or "") .. " files")
  end,
  grep = function(block)
    return layout.text(smelt.text.line_count((block.output and block.output.content) or "") .. " matches")
  end,
  read_file = function(block)
    return layout.text(smelt.text.line_count((block.output and block.output.content) or "") .. " lines")
  end,
  web_fetch = function(block, ctx)
    local items = {}
    local args = block.args or {}
    if args.prompt and args.prompt ~= "" then
      items[#items + 1] = layout.text(args.prompt)
    end
    items[#items + 1] = M.render_tool_output(block.output, ctx)
    return layout.vbox(items)
  end,
  write_file = function(block)
    local args = block.args or {}
    return layout.file_view({
      content = args.content or "",
      path = args.file_path or "",
    })
  end,
}

--- Render a tool body. Raw output is the safe default when no tool-specific
--- structured renderer is available.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts: table?): table?
function smelt.transcript.defaults.render_tool_body(block, ctx, opts)
  opts = opts or {}
  local renderer = tool_body_renderers[block.name or ""]
  if not block.output and not renderer then return nil end

  local output = block.output or { content = "", is_error = false }
  if output.is_error then return M.render_tool_output(output, ctx, opts) end

  if renderer then
    local render_block = block
    if not block.output then
      render_block = setmetatable({ output = output }, { __index = block })
    end
    local body = renderer(render_block, ctx, opts)
    if not body then return nil end
    local limits = (ctx and ctx.limits) or {}
    return layout.cap(
      layout.gutter(body, { text = opts.gutter or "  " }),
      {
        rows = opts.rows or limits.tool_body_rows or 20,
        keep = opts.keep or "head",
        marker = opts.marker,
      }
    )
  end
  return M.render_tool_output(block.output, ctx, opts)
end

--- Render raw tool output using generic layout primitives: text, gutter, and a
--- rendered-row cap. Error output uses `ErrorMsg`; success output remains dimmed
--- by the text primitive's no-highlight fallback.
---@type fun(output: smelt.transcript.ToolOutput?, ctx: smelt.transcript.Context?, opts: table?): table
function smelt.transcript.defaults.render_tool_output(output, ctx, opts)
  opts = opts or {}
  ctx = ctx or {}
  local limits = ctx.limits or {}
  local content = output and output.content or ""
  local is_error = output and output.is_error == true
  local rows = opts.rows or limits.tool_output_rows or 20
  local hl = opts.hl or opts.hl_group
  if not hl and is_error then hl = "ErrorMsg" end

  return layout.cap(
    layout.gutter(
      layout.text(content, {
        hl_group = hl,
        ansi = true,
      }),
      { text = opts.gutter or "  " }
    ),
    {
      rows = rows,
      keep = opts.keep or "tail",
      marker = opts.marker or "above",
    }
  )
end

--- Render a user block. Custom renderers can layer richer panel/text
--- annotations; the bundled default keeps the renderer path total and
--- fallback-safe with current primitives.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_user(block, ctx)
  return M.render_user_text(block, ctx)
end

--- Render user text.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_user_text(block, ctx)
  local _ = ctx
  return layout.text(block.text or "")
end

--- Render assistant text.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_assistant(block, ctx)
  local _ = ctx
  return layout.text(block.content or "")
end

--- Render thinking, either expanded with the current gutter or folded to a
--- deterministic text summary when `ctx.show_thinking` is false.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_thinking(block, ctx)
  ctx = ctx or {}
  if not ctx.show_thinking then return M.render_thinking_summary(block, ctx) end
  return layout.gutter(layout.text(block.content or ""), { text = "│ " })
end

--- Render a compact thinking summary.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_thinking_summary(block, ctx)
  local _ = ctx
  return layout.text(block.thinking_summary or "thinking")
end

--- Render an exec block.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_exec(block, ctx)
  local _ = ctx
  local items = {}
  items[#items + 1] = layout.text("!" .. (block.command or ""))
  if block.output and block.output ~= "" then
    items[#items + 1] = layout.gutter(
      layout.text(block.output, { ansi = true }),
      { text = "  " }
    )
  end
  return layout.vbox(items)
end

--- Render a mode note.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_mode(block, ctx)
  local _ = ctx
  return layout.text((block.icon or "") .. (block.text or ""), {
    hl = block.hl_group or "SmeltModeDefault",
  })
end

--- Render a process-status note.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_process_status(block, ctx)
  local _ = ctx
  return layout.text(block.text or "", { hl = block.hl_group or "SmeltProcess" })
end

--- Render a compacted-history marker.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_compacted(block, ctx)
  local _ = ctx
  local items = {}
  items[#items + 1] = layout.text(" compacted ")
  if block.summary and block.summary ~= "" then
    items[#items + 1] = layout.text(block.summary)
  end
  return layout.vbox(items)
end

--- Render a code block with the current primitive slice.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_code(block, ctx)
  local _ = ctx
  return layout.text(block.content or "")
end

--- Render unknown block kinds without failing the transcript.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_unknown(block, ctx)
  local _ = ctx
  return layout.text(block.content or block.text or block.summary or "")
end

package.loaded["smelt.transcript.defaults"] = M

return M
