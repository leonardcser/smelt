-- The transcript as a timeline: a rail down the left, a node on it for each message and tool.
local transcript, L = smelt.transcript, smelt.layout
-- The timeline node replaces the tool marker, not its label or timings.
transcript.set_renderer(function(block, ctx)
  return transcript.render_part("tool_header_content", block, ctx)
end, { part = "tool_header" })
transcript.set_renderer(function(block, ctx)
  return L.style(transcript.render_part("user_content", block, ctx), { bold = true })
end, { part = "user" })
local STATUS = { ok = "SmeltSuccess", err = "ErrorMsg", denied = "ErrorMsg", confirm = "SmeltAccent" }
transcript.extend_renderer("timeline", function(next, block, ctx)
  local glyph, hl = "○", "SmeltHeading"
  if block.kind == "user" then
    glyph, hl = "●", "SmeltAccent"
  elseif block.kind == "tool" then
    glyph, hl = "◆", STATUS[block.status] or "SmeltToolPending"
  elseif block.kind == "group" then
    glyph, hl = "◆", "SmeltSuccess"
  end
  return L.row_prefix(next(block, ctx), {
    first = { { text = " " .. glyph .. " ", hl = hl, bold = true, selectable = false } },
    rest = { { text = " │ ", hl = "Comment", selectable = false } },
  })
end)
