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

local function elapsed_suffix(secs)
  if not secs then return nil end
  if secs < 60 then
    return string.format("%ds", secs)
  elseif secs < 3600 then
    return string.format("%dm %ds", secs // 60, secs % 60)
  end
  local h = secs // 3600
  local rest = secs % 3600
  return string.format("%dh %dm %ds", h, rest // 60, rest % 60)
end

local function first_non_empty_line(s)
  s = s or ""
  for line in (s .. "\n"):gmatch("([^\n]*)\n") do
    if line:match("%S") then return (line:gsub("^%s+", ""):gsub("%s+$", "")) end
  end
  return ""
end

local function line_count(s)
  local n = 0
  for line in (s or ""):gmatch("[^\n]+") do
    if line:match("%S") then n = n + 1 end
  end
  return n
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

--- Render the default one-line tool header.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts: table?): table
function smelt.transcript.defaults.render_tool_header(block, ctx, opts)
  local _ = ctx
  opts = opts or {}
  local name = block.name or "tool"
  local summary = block.summary_text or ""
  local parts = { "* ", name }
  if summary ~= "" then
    parts[#parts + 1] = " "
    parts[#parts + 1] = summary
  end
  local elapsed = block.status ~= "confirm" and elapsed_suffix(block.elapsed_secs) or nil
  if elapsed then
    parts[#parts + 1] = " ("
    parts[#parts + 1] = elapsed
    parts[#parts + 1] = ")"
  end
  return layout.text(table.concat(parts), {
    hl = opts.hl or opts.hl_group or status_hl[block.status or "pending"],
  })
end

--- Render a tool body. Raw output is the safe default when no tool-specific
--- structured renderer is available.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts: table?): table?
function smelt.transcript.defaults.render_tool_body(block, ctx, opts)
  if not block.output then return nil end
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
  local content = block.content or ""
  local first = first_non_empty_line(content)
  local n = line_count(content)
  if first == "" then return layout.text("thinking") end
  if n <= 1 then return layout.text("thinking: " .. first) end
  return layout.text(string.format("thinking: %s (+%d lines)", first, n - 1))
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
