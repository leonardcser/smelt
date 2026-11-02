-- Lightning when something breaks: a tool that fails or a turn that errors sends a bolt forking down
-- the screen. It glows white-hot, cools to violet and is gone, so you notice from across the room.
local LIFE = 1.1
local bolts, paint = {}, nil

--- A bolt from the top to `h` rows down near column `x`, forking once on the way.
local function fork(x, top, h, out)
  for y = top, h - 1 do
    local step = math.random(-1, 1)
    out[#out + 1] = { x, y, step < 0 and "╱" or step > 0 and "╲" or "│" }
    x = x - step
    if y > top + 3 and math.random() < 0.04 and not out.forked then
      out.forked = true
      local branch = { forked = true }
      fork(x, y + 1, math.min(h, y + math.random(4, 9)), branch)
      for _, p in ipairs(branch) do out[#out + 1] = p end
    end
  end
  return out
end

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  local live = {}
  for _, b in ipairs(bolts) do
    b.pts = b.pts or fork(math.random(8, math.max(9, s:width() - 8)), 0, s:height() - 3, {})
    b.at = b.at or now
    local k = (now - b.at) / LIFE
    if k < 1 then
      live[#live + 1] = b
      local heat = (1 - k) ^ 2
      local color = { 170 + 85 * heat, 150 + 105 * heat, 255 }
      -- The bolt grows down from the sky in its first moment.
      local reach = math.min(1, k * 8) * #b.pts
      for i = 1, reach do
        local p = b.pts[i]
        if p[1] >= 0 and p[1] < s:width() then s:set(p[1], p[2], p[3], { fg = color, bold = heat > 0.5 }) end
      end
    end
  end
  bolts = live
  if #bolts > 0 then paint:invalidate_after(33) end
end, { name = "lightning", order = 900 })

local function strike()
  bolts[#bolts + 1] = {}
  paint:invalidate()
end
smelt.events.on("tool_end", function(e)
  if e.is_error then strike() end
end)
smelt.events.on("turn_error", strike)
smelt.cmd.register("lightning", strike, { desc = "strike once" })
