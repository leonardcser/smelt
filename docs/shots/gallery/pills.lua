-- A statusline of rounded pills: the mode, the project, the model and the time.
local BAR = { 30, 30, 46 }
local COLORS = { { 245, 194, 231 }, { 137, 180, 250 }, { 166, 227, 161 }, { 249, 226, 175 } }
local function pill(text, color, right)
  return { runs = {
    { text = "\u{e0b6}", style = { fg = color } },
    { text = " " .. text .. " ", style = { fg = BAR, bg = color, bold = true } },
    { text = "\u{e0b4}", style = { fg = color } },
  }, align_right = right }
end
smelt.statusline:clear()
smelt.statusline:add("pills", function()
  return {
    pill(smelt.signal.get("agent_mode") or "normal", COLORS[1]),
    pill(smelt.path.basename(smelt.os.cwd()), COLORS[2]),
    pill(((smelt.signal.get("model") or ""):gsub("^.*/", "")), COLORS[3]),
    pill(os.date("%H:%M"), COLORS[4], true),
  }
end)
smelt.timer.every(1000, function() smelt.statusline:invalidate() end)
