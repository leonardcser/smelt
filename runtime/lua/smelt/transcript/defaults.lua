local M = {}

local layout = smelt.layout

function M.render_tool_output(output, ctx, opts)
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

return M
