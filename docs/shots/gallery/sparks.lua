-- Sparks fly from the prompt as you type.
local bits = {}
local paint = smelt.paint.register(function(s)
  for _, b in ipairs(bits) do
    local x, y = math.floor(b[1]), math.floor(b[2])
    if x >= 0 and x < s:width() and y >= 0 and y < s:height() then
      local heat = b[5] / 14
      s:set(x, y, heat > 0.6 and "✦" or heat > 0.3 and "•" or "·", { fg = { 255, math.floor(90 + 160 * heat), math.floor(40 * heat) } })
    end
  end
end)
smelt.overlay.new({
  anchor = "screen_at", row = 0, col = 0, width = "100%", height = "100%",
  layout = smelt.ui.layout.leaf(paint), border = "none", transparent = true, z = 200,
})
smelt.prompt.win:on("text_changed", function()
  local size = smelt.terminal.size()
  local x = 2 + smelt.text.width(smelt.prompt.text()) % (size.width - 4)
  for _ = 1, 6 do
    bits[#bits + 1] = { x, size.height - 3, (math.random() - 0.5) * 1.6, -math.random() * 1.2 - 0.3, 14 }
  end
end)
smelt.timer.every(40, function()
  for i = #bits, 1, -1 do
    local b = bits[i]
    b[1], b[2], b[4], b[5] = b[1] + b[3], b[2] + b[4], b[4] + 0.12, b[5] - 1
    if b[5] <= 0 then table.remove(bits, i) end
  end
end)
