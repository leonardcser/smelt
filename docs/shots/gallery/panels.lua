-- Your messages and every tool call on a panel: a band of background with a row of padding above
-- and below, each kind of tool in a color of its own. Answers stay plain text between them.
local transcript, L = smelt.transcript, smelt.layout
transcript.set_renderer(function(block, ctx)
  return transcript.render_part("tool_header_content", block, ctx)
end, { part = "tool_header" })

local function mix(a, b, k)
  return { a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k, a[3] + (b[3] - a[3]) * k }
end
local bg = smelt.theme.rgb("Normal", "bg")

-- A tint for each kind of tool: shell green, edits amber, reads blue, the rest violet.
local KINDS = {
  shell = { { 80, 200, 120 }, { "bash" } },
  edit = { { 240, 170, 60 }, { "edit_file", "write_file" } },
  read = { { 90, 150, 255 }, { "read_file", "grep", "glob", "web_fetch", "web_search" } },
  other = { { 190, 120, 255 }, {} },
}
local GROUP = {}
for kind, spec in pairs(KINDS) do
  local hl = "Panel" .. kind:gsub("^%l", string.upper)
  smelt.theme.set(hl, { bg = mix(bg, spec[1], 0.16) })
  for _, tool in ipairs(spec[2]) do GROUP[tool] = hl end
end

transcript.extend_renderer("panels", function(next, block, ctx)
  return L.panel(next(block, ctx), { hl = GROUP[block.name] or "PanelOther", padding = 1 })
end, { part = "tool" })

transcript.extend_renderer("panels", function(next, block, ctx)
  return L.panel(next(block, ctx), { hl = "PanelRead", padding = 1 })
end, { part = "group" })
