smelt.transcript = smelt.transcript or {}
smelt.transcript.defaults = smelt.transcript.defaults or {}

local M = smelt.transcript.defaults
M.__tool_body_renderers = M.__tool_body_renderers or {}

local layout = smelt.layout

local status_hl = {
  pending = "SmeltToolPending",
  ok = "SmeltSuccess",
  err = "ErrorMsg",
  denied = "ErrorMsg",
  confirm = "SmeltAccent",
}

function M.display_count_text(block, opts)
  opts = opts or {}
  local output = block and block.output or {}
  local metadata = output.metadata or {}
  local display_count = metadata.display_count
  if type(display_count) ~= "table" then display_count = {} end
  local count = tonumber(display_count.value)
  if count == nil then count = tonumber(opts.count) or 0 end
  local unit = display_count.unit or opts.unit or "item"
  local plural = display_count.plural or opts.plural
  if plural == nil then
    if unit == "match" then
      plural = "matches"
    else
      plural = unit .. "s"
    end
  end
  return count .. " " .. (count == 1 and unit or plural)
end

function M.render_display_count(block, opts)
  return layout.text(M.display_count_text(block, opts))
end

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
local tool_header_indent
local tool_header_prefix

--- Render the default one-line tool header.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts: table?): table
function smelt.transcript.defaults.render_tool_header(block, ctx, opts)
  local _ = ctx
  opts = opts or {}
  local status = block.status or "pending"
  local hl = opts.hl or opts.hl_group or block.status_hl or status_hl[status]
  local lines = tool_header_lines(block, status, hl)
  local header = layout.runs(lines, { continuation_indent = tool_header_indent(block) })
  local show_elapsed = block.elapsed
    and status ~= "confirm"
    and (status == "pending" or (block.elapsed_text and block.elapsed_text ~= ""))
  if show_elapsed then
    header = layout.hbox({
      { header, fit = true },
      { layout.line({ { text = "  ", selectable = false, dim = true } }), cols = 2 },
      { layout.elapsed(block.elapsed, { dim = true, selectable = false }), cols = 8 },
    })
  end
  return layout.cap(header, {
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

local function display_width(s)
  if smelt.text and smelt.text.width then
    local ok, width = pcall(smelt.text.width, s)
    if ok and type(width) == "number" then return width end
  end
  return #s
end

function tool_header_prefix(block, hl, tail, has_summary)
  local tool_name = block.name or "tool"
  local spans = {
    { text = "*", hl = hl },
    { text = " " .. tool_name, dim = true },
  }
  if tail then spans[#spans + 1] = { text = " ", selectable = has_summary } end
  return spans, display_width("* " .. tool_name .. " ")
end

function tool_header_indent(block)
  local _, width = tool_header_prefix(block, nil, true, false)
  return width
end

function tool_header_lines(block, status, hl)
  local lines = summary_lines(block.summary)
  local suffix = {}

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
  local prefix, prefix_width = tool_header_prefix(block, hl, tail, has_summary)
  for i = #prefix, 1, -1 do
    table.insert(first, 1, prefix[i])
  end

  if has_summary and #lines > 1 then
    local indent = string.rep(" ", prefix_width)
    for i = 2, #lines do
      table.insert(lines[i], 1, { text = indent, selectable = false, dim = true })
    end
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

--- Render a tool body. Raw output is capped by the safe tail-output helper;
--- structured renderers are guttered but otherwise left uncapped.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts: table?): table?
function smelt.transcript.defaults.render_tool_body(block, ctx, opts)
  opts = opts or {}
  local renderer = M.__tool_body_renderers[block.name or ""]
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
    return layout.gutter(body, { text = opts.gutter or "  " })
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

  return layout.gutter(
    layout.cap(
      layout.text(content, {
        hl_group = hl,
        ansi = true,
      }),
      {
        rows = rows,
        keep = opts.keep or "tail",
        marker = opts.marker or "above",
      }
    ),
    { text = opts.gutter or "  " }
  )
end

--- Render a user block. Custom renderers can layer richer panel/text
--- annotations; the bundled default keeps the same full-width prompt chrome as
--- the Rust renderer while leaving the content policy in Lua.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_user(block, ctx)
  return layout.panel(M.render_user_text(block, ctx), {
    hl = "SmeltUserBg",
    padding = 1,
  })
end

--- Render user text.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_user_text(block, ctx)
  local _ = ctx
  return layout.runs(block.user_lines or block.text or "")
end

--- Render assistant text.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_assistant(block, ctx)
  local _ = ctx
  return layout.markdown(block.content or "")
end

--- Render thinking, either expanded with the current gutter or folded to a
--- deterministic text summary when `ctx.show_thinking` is false.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_thinking(block, ctx)
  ctx = ctx or {}
  if not ctx.show_thinking then return M.render_thinking_summary(block, ctx) end
  return layout.gutter(
    layout.markdown(block.content or "", {
      dim = true,
      italic = true,
      inline = true,
    }),
    { text = "│ ", styled = true }
  )
end

--- Render a compact thinking summary.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_thinking_summary(block, ctx)
  local _ = ctx
  return layout.gutter(
    layout.markdown(block.thinking_summary or "thinking (0 lines)", {
      dim = true,
      italic = true,
      inline = true,
    }),
    { text = "│ ", styled = true }
  )
end

--- Render an exec block.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_exec(block, ctx)
  ctx = ctx or {}
  local items = {}
  local command_spans = block.command_spans or {
    { text = "!", fg = "SmeltExecPrefix", bold = true },
    { text = block.command or "", bold = true },
  }
  items[#items + 1] = layout.panel(
    layout.runs({ command_spans }),
    { hl = "SmeltUserBg", padding = 1 }
  )
  if block.output and block.output ~= "" then
    local limits = ctx.limits or {}
    items[#items + 1] = layout.gutter(
      layout.cap(
        layout.text(block.output, { ansi = true }),
        {
          rows = limits.tool_output_rows or 20,
          keep = "tail",
          marker = "above",
        }
      ),
      { text = "  ", styled = true }
    )
  end
  return layout.vbox(items)
end

--- Render a mode note.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_mode(block, ctx)
  local _ = ctx
  local hl = block.hl_group or "SmeltModeDefault"
  return layout.line({
    { text = block.icon or "", fg = hl },
    { text = block.text or "", fg = hl, italic = true },
  })
end

--- Render a process-status note.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_process_status(block, ctx)
  local _ = ctx
  local hl = block.hl_group or "SmeltProcess"
  return layout.runs({ {
    { text = block.text or "", fg = hl, italic = true },
  } })
end

--- Render a compacted-history marker.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_compacted(block, ctx)
  local _ = ctx
  local items = {}
  items[#items + 1] = layout.separator({ label = " compacted ", dim = true })
  if block.summary and block.summary ~= "" then
    items[#items + 1] = layout.markdown(block.summary, { dim = true })
  end
  return layout.vbox(items)
end

--- Render a code block with syntax highlighting.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_code(block, ctx)
  local _ = ctx
  return layout.code(block.content or "", { lang = block.lang or "" })
end

--- Render unknown block kinds without failing the transcript.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table
function smelt.transcript.defaults.render_unknown(block, ctx)
  local _ = ctx
  return layout.text(block.content or block.text or block.summary or "")
end

package.loaded["smelt.transcript.defaults"] = M

return M
