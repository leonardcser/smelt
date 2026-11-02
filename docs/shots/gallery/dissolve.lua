-- Dissolve: when most of the screen changes at once (a theme, `/clear`, a resumed session), the old
-- screen breaks up into the new one grain by grain instead of cutting. Scrolling doesn't count.
local fx = require("fx")

local LIFE = 0.45
local SHARE = 0.3
-- The screen being dissolved away, and what changed.
local old, paint = nil, nil

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  local w, h = s:width(), s:height()
  if not old and ctx.prev and ctx.prev:width() == w and ctx.prev:height() == h and #ctx.scrolls == 0 then
    local changed = 0
    for _, d in ipairs(ctx.damage) do changed = changed + d.width end
    if changed > SHARE * w * h then old = { snap = ctx.prev, damage = ctx.damage, start = now } end
  end
  if not old then return end
  local t = (now - old.start) / LIFE
  if t >= 1 then
    old = nil
    return
  end
  paint:invalidate_after(16)
  for _, d in ipairs(old.damage) do
    for x = d.x, d.x + d.width - 1 do
      local o = old.snap:get(x, d.y)
      if fx.hash(d.y * w + x) > t and o.width == 1 and s:get(x, d.y).width == 1 then
        s:set(x, d.y, o.ch, o.style)
      end
    end
  end
end, { name = "dissolve", history = true })
