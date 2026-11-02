-- The transcript as a messenger: your messages in blue bubbles on the right, smelt's answers in
-- gray ones on the left, each as wide as its text. Everything else, tool calls included, shows as
-- it always does.
local transcript, L = smelt.transcript, smelt.layout
local function mix(a, b, k)
  return { a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k, a[3] + (b[3] - a[3]) * k }
end
local bg, fg = smelt.theme.rgb("Normal", "bg"), smelt.theme.rgb("Normal", "fg")
smelt.theme.set("BubbleYou", { fg = "#ffffff", bg = "#2f72e8" })
smelt.theme.set("BubbleSmelt", { fg = fg, bg = mix(bg, fg, 0.13) })

-- A bubble fits its text and leaves a margin on the far side, so long ones wrap short of the edge.
local function bubble(body, hl, right)
  local b = { L.panel(body, { hl = hl, padding = 1 }), fit = true, copy_owner = true }
  local margin, gap = { L.empty(), cols = 8 }, { L.empty(), weight = 1 }
  return L.hbox(right and { margin, gap, b } or { b, gap, margin })
end

transcript.set_renderer(function(block, ctx)
  local content = transcript.render_part("user_content", block, ctx)
  return bubble(L.style(content, { hl = "BubbleYou" }), "BubbleYou", true)
end, { part = "user" })

transcript.extend_renderer("bubbles", function(next, block, ctx)
  return bubble(next(block, ctx), "BubbleSmelt", false)
end, { part = "assistant" })
