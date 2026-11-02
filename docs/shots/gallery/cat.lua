-- A cat in the corner that keeps an eye on the agent: it dozes while nothing happens, watches the
-- work with its eyes going back and forth, and is pleased for a moment when the turn is done.
local buf = smelt.buf.new()
local PLEASED_MS = 4000
local frame, pleased_until = 0, 0

local function draw()
  local now = smelt.time.monotonic_ms()
  local state = smelt.signal.get("work_state")
  local face, above
  if state == "working" or state == "busy" then
    face = ({ "(o.o )", "( o.o)" })[frame % 2 + 1]
    above = "      "
  elseif now < pleased_until then
    face, above = "( ^.^)", "   ♪  "
  else
    face = "( -.-)"
    above = ({ "    z ", "   z  ", "  Z   " })[frame % 3 + 1]
  end
  buf:lines({ above, [[ /\_/\]], face, " > ^ <" })
end

draw()
local win = smelt.win.new(buf, { surface = "inert", scrollbar = false })
smelt.overlay.new({
  name = "cat", anchor = "screen_at", corner = "se",
  row = 4, col = 2, modal = false, border = "none",
  layout = smelt.ui.layout.leaf(win, { measure = { 7, 4 } }),
})
smelt.timer.every(900, function()
  frame = frame + 1
  draw()
end)
smelt.events.on("turn_end", function()
  pleased_until = smelt.time.monotonic_ms() + PLEASED_MS
  draw()
end)
smelt.signal.subscribe("work_state", draw)
