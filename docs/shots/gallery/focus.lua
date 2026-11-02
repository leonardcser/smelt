-- Focus: the message or tool under the mouse stays sharp and the rest of the conversation fades
-- back. `/focus` turns it off and on.
local fx = require("fx")

local DIM = 0.68
local on = true
-- How faded each block is, by block id, easing toward its target.
local level = {}
local paint

paint = smelt.paint.frame(function(s, ctx)
  local blocks = fx.blocks()
  local p = on and ctx.pointer
  local view = smelt.transcript.win:rect()
  -- The block under the mouse, or the nearest one above or below when it points between them.
  local hover, near = nil, math.huge
  if p and view and fx.inside(view, p.x, p.y) then
    for _, b in ipairs(blocks) do
      local d = math.max(b.rect.y - p.y, p.y - (b.rect.y + b.rect.height - 1), 0)
      if d < near then hover, near = b.block_id, d end
    end
  end
  local moving, seen = false, {}
  for _, b in ipairs(blocks) do
    local want = (hover and b.block_id ~= hover) and DIM or 0
    local cur = level[b.block_id] or 0
    if math.abs(want - cur) < 0.02 then cur = want else cur, moving = cur + (want - cur) * 0.35, true end
    seen[b.block_id] = cur
    if cur > 0 then
      local r = b.rect
      for y = r.y, r.y + r.height - 1 do
        for x = r.x, r.x + r.width - 1 do
          local c = s:get(x, y)
          if c.ch ~= " " and c.ch ~= "" then
            local fg, bg = fx.rgb(c.style.fg, "fg"), fx.rgb(c.style.bg, "bg")
            s:patch_style(x, y, 1, 1, { fg = fx.mix(fg, bg, cur) })
          end
        end
      end
    end
  end
  level = seen
  if moving then paint:invalidate_after(33) end
end, { name = "focus", pointer = true })

smelt.cmd.register("focus", function()
  on = not on
  paint:invalidate()
  smelt.notify.info(on and "focus on" or "focus off")
end, { desc = "fade everything but the message under the mouse" })
