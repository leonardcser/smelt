-- Windows 95: grey everywhere, a blue title bar with its buttons, and a taskbar with Start.
local SILVER, NAVY, WHITE, BLACK = { 192, 192, 192 }, { 0, 0, 128 }, { 255, 255, 255 }, { 0, 0, 0 }
local spec = smelt.theme.spec("github")
spec.groups.Normal = { fg = BLACK, bg = SILVER }
spec.groups.SmeltUserBg = { bg = WHITE }
spec.groups.SmeltCodeBlockBg = { bg = WHITE }
spec.groups.SmeltStatusBg = { bg = SILVER }
smelt.theme.apply(spec)

smelt.headerline:add("titlebar", function(ctx)
  local title = " ▣ " .. smelt.path.basename(smelt.os.cwd()) .. " - smelt"
  local buttons = "[_][□][×]"
  local gap = (" "):rep(math.max(1, ctx.width - smelt.text.width(title) - smelt.text.width(buttons)))
  return { runs = {
    { text = title .. gap, style = { fg = WHITE, bg = NAVY, bold = true } },
    { text = buttons, style = { fg = BLACK, bg = SILVER, bold = true } },
  } }
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
