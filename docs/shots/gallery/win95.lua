-- Windows 95: grey everywhere, a blue title bar with a working close button, and a taskbar with Start.
local SILVER, NAVY, WHITE, BLACK = { 192, 192, 192 }, { 0, 0, 128 }, { 255, 255, 255 }, { 0, 0, 0 }
local spec = smelt.theme.spec("github")
spec.groups.Normal = { fg = BLACK, bg = SILVER }
spec.groups.SmeltUserBg = { bg = WHITE }
spec.groups.SmeltCodeBlockBg = { bg = WHITE }
spec.groups.SmeltStatusBg = { bg = SILVER }
smelt.theme.apply(spec)

smelt.headerline:set_view(function()
  local layout = smelt.ui.layout
  local function region(text, style)
    local buf = smelt.buf.new()
    buf:lines({ text })
    return smelt.win.new(buf, { surface = "inert", scrollbar = false, wrap = false, style = style })
  end
  local title = region(" ▣ " .. smelt.path.basename(smelt.os.cwd()) .. " - smelt", { fg = WHITE, bg = NAVY, bold = true })
  local buttons = region("[_][□]", { fg = BLACK, bg = SILVER, bold = true })
  local close = region("[×]", { fg = BLACK, bg = SILVER, bold = true })
  close:on("press", function(event)
    if event.button == "left" then smelt.quit() end
  end)
  return layout.hbox({
    { layout.leaf(title), width = "fill" },
    { layout.leaf(buttons), width = "fit" },
    { layout.leaf(close), width = "fit" },
  })
end)
smelt.statusline:clear()
smelt.statusline:add("taskbar", function()
  return {
    { text = " ⊞ Start ", style = { fg = BLACK, bg = { 223, 223, 223 }, bold = true } },
    { text = " ▣ smelt ", style = { fg = BLACK, bg = { 210, 210, 210 } } },
    { text = " " .. (smelt.signal.get("agent_mode") or "normal") .. " ", style = { fg = BLACK, bg = { 223, 223, 223 } }, align_right = true },
    { text = " " .. os.date("%H:%M") .. " ", style = { fg = BLACK, bg = { 223, 223, 223 } }, align_right = true },
  }
end)
smelt.timer.every(1000, function() smelt.statusline:invalidate() end)
