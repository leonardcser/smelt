-- The headerline as a marquee: the project, the time and a word from smelt scrolling past.
local t = 0
smelt.headerline:add("marquee", function(ctx)
  local width = ctx.width
  local msg = "   ◆ now showing: acme-app   ◆ " .. os.date("%H:%M") .. "   ◆ smelt, yours to change   "
  local chars = {}
  for _, c in utf8.codes(msg) do chars[#chars + 1] = utf8.char(c) end
  local out = {}
  for i = 1, width do out[i] = chars[(t + i - 1) % #chars + 1] end
  local text = table.concat(out)
  return { text = text, truncatable = true, style = { fg = { 255, 210, 90 }, bg = { 60, 20, 70 }, bold = true } }
end)
smelt.timer.every(90, function()
  t = t + 1
  smelt.headerline:invalidate()
end)
