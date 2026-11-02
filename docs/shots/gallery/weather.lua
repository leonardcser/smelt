-- /weather <rain|snow|autumn|spring|off>: weather over the conversation that knows where the text
-- is. Rain splashes on words, snow piles up on them, leaves and petals land on them for a while.
-- It eases in, and when you turn it off it eases out and what lies on the text melts away.
local fx = require("fx")

local KINDS = {
  rain = {
    rate = 1.6, speed = { 22, 30 }, sway = 0, glyphs = { "│", "╎" },
    colors = { { 110, 150, 240 }, { 80, 115, 200 } }, land = "splash", stick = 0.15,
  },
  snow = {
    rate = 1 / 1.8, speed = { 3, 6 }, sway = 0.6, glyphs = { "·", "*" },
    colors = { { 190, 200, 230 }, { 240, 245, 255 } }, land = "pile", stick = 0.06,
  },
  autumn = {
    rate = 1 / 7, speed = { 1.6, 3 }, sway = 1.8, glyphs = { "❦", "❧" },
    colors = { { 214, 110, 40 }, { 190, 70, 40 }, { 226, 160, 60 }, { 160, 100, 50 } }, land = "rest", stick = 0.05,
  },
  spring = {
    rate = 1 / 6, speed = { 1.2, 2.4 }, sway = 1.4, glyphs = { "✿", "❀", "·" },
    colors = { { 255, 130, 180 }, { 250, 100, 160 }, { 255, 150, 205 } }, land = "rest", stick = 0.05,
  },
}
local SNOW = { 240, 245, 255 }
local PILE = { "▁", "▂", "▃" }
-- A light dusting is faint; deeper snow is brighter.
local DEPTH = { 0.45, 0.75, 1 }
-- Seconds a leaf or petal rests on a word, and a splash shows. Each kind's `stick` is the chance it
-- lands on a word it meets; otherwise it falls on behind the text.
local REST, SPLASH = 7, 0.15

-- The weather falling, how strong it is (eases toward `target`), what fell, what lies on the text
-- by block id and "row:x" of the glyph under it, and splashes in screen cells.
local kind, level, target, seed = nil, 0, 0, false
local drops, lying, splashes, last, paint = {}, {}, {}, nil, nil

local function pick(list) return list[math.random(#list)] end

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  local dt = math.min(0.1, last and now - last or 0)
  last = now
  if not kind then return end
  level = level + (target - level) * math.min(1, dt * 1.2)
  if target == 0 and level < 0.02 and #drops == 0 and next(lying) == nil and #splashes == 0 then
    kind, level = nil, 0
    return
  end
  paint:invalidate_after(kind.land == "splash" and 33 or 66)
  local view = smelt.transcript.win:rect()
  if not view then return end

  local at, by_id = {}, {}
  for _, b in ipairs(fx.blocks()) do
    by_id[b.block_id] = b
    for y = b.rect.y, b.rect.y + b.rect.height - 1 do at[y] = b end
  end
  --- The block whose text is at `(x, y)`, and the glyph's row in it, or nil off the text.
  local function text(x, y)
    local b = at[y]
    if not b or x < b.rect.x or x >= b.rect.x + b.rect.width then return nil end
    local c = s:get(x, y)
    if c.ch == " " or c.ch == "" then return nil end
    return b, b.top + y - b.rect.y
  end
  local function empty(x, y)
    local c = fx.inside(view, x, y) and s:get(x, y)
    return c and c.ch == " "
  end
  --- Where something lying at `key` in block `b` shows on screen, or nil once scrolled off.
  local function spot(b, key)
    local row, x = key:match("(%d+):(%d+)")
    local y = b.rect.y + tonumber(row) - b.top
    if y < b.rect.y or y >= b.rect.y + b.rect.height then return nil end
    return tonumber(x), y
  end

  -- What lies on a glyph that went, or that something now covers, falls again; leaves and petals
  -- blow off after a while, and once the weather stops snow melts a layer at a time.
  for id, pile in pairs(lying) do
    local b = by_id[id]
    if not b then pile = {} end
    for key, l in pairs(pile) do
      local x, y = spot(b, key)
      if x and (not text(x, y) or not empty(x, y - 1)) then
        pile[key] = nil
        drops[#drops + 1] = { x = x + 0.5, y = y - 1, v = kind.speed[1], p = math.random() * 6, g = l.g, c = l.c }
      elseif l.until_s and now > l.until_s then
        pile[key] = nil
      elseif target == 0 and math.random() < dt * 0.8 then
        l.n = l.n - 1
        if l.n <= 0 then pile[key] = nil end
      end
    end
    if next(pile) == nil then lying[id] = nil end
  end

  -- Weather that starts is already falling all the way down, at the rate it will keep.
  local fresh = {}
  if seed then
    seed = false
    local fall = view.height / ((kind.speed[1] + kind.speed[2]) / 2)
    for _ = 1, math.floor(view.width * kind.rate * fall) do fresh[#fresh + 1] = view.y + math.random() * view.height end
  end
  local spawn = target > 0 and math.floor(view.width * kind.rate * level * dt + math.random()) or 0
  for _ = 1, spawn do fresh[#fresh + 1] = view.y end
  for _, y0 in ipairs(fresh) do
    drops[#drops + 1] = {
      x = view.x + math.random() * view.width, y = y0,
      v = kind.speed[1] + math.random() * (kind.speed[2] - kind.speed[1]),
      p = math.random() * 6, g = pick(kind.glyphs), c = pick(kind.colors),
    }
  end

  --- Land on the glyph at `(x, y)`; false when there is no room there.
  local function land(x, y, d)
    local b, row = text(x, y)
    if not b or not empty(x, y - 1) then return false end
    local pile = lying[b.block_id] or {}
    lying[b.block_id] = pile
    local key = row .. ":" .. x
    local l = pile[key]
    if kind.land == "pile" then
      if l and l.n >= #PILE then return false end
      pile[key] = { n = (l and l.n or 0) + 1, g = d.g, c = d.c }
    else
      if l then return false end
      pile[key] = { n = 1, g = d.g, c = d.c, until_s = now + REST * (0.6 + math.random() * 0.8) }
    end
    return true
  end

  local kept = {}
  for _, d in ipairs(drops) do
    d.y = d.y + d.v * dt
    d.x = d.x + math.sin(now * 1.7 + d.p) * kind.sway * dt
    local x, y = math.floor(d.x), math.floor(d.y)
    if y + 1 >= view.y + view.height or x < view.x or x >= view.x + view.width then
      -- Gone at the bottom and off the sides.
    elseif empty(x, y) and text(x, y + 1) and math.random() < kind.stick then
      if kind.land == "splash" then
        splashes[#splashes + 1] = { x = x, y = y, until_s = now + SPLASH }
      elseif not land(x, y + 1, d) and not land(x - 1, y + 1, d) and not land(x + 1, y + 1, d) then
        kept[#kept + 1] = d
      end
    else
      kept[#kept + 1] = d
    end
  end
  drops = kept

  for id, pile in pairs(lying) do
    local b = by_id[id]
    for key, l in pairs(pile) do
      local x, y = spot(b, key)
      if x and empty(x, y - 1) then
        if kind.land == "pile" then
          local bg = fx.rgb(s:get(x, y - 1).style.bg, "bg")
          s:set(x, y - 1, PILE[l.n], { fg = fx.mix(bg, SNOW, DEPTH[l.n]) })
        else
          s:set(x, y - 1, l.g, { fg = l.c })
        end
      end
    end
  end
  local live = {}
  for _, sp in ipairs(splashes) do
    if now < sp.until_s then
      live[#live + 1] = sp
      if empty(sp.x, sp.y) then s:set(sp.x, sp.y, "˙", { fg = KINDS.rain.colors[1] }) end
    end
  end
  splashes = live
  for _, d in ipairs(drops) do
    local x, y = math.floor(d.x), math.floor(d.y)
    if empty(x, y) then s:set(x, y, d.g, { fg = d.c }) end
  end
end, { name = "weather" })

smelt.cmd.register("weather", function(arg)
  local name = (arg or ""):match("%S+") or ""
  local next_kind = KINDS[name]
  if next_kind and next_kind ~= kind then
    drops, lying, splashes = {}, {}, {}
    kind, level, seed = next_kind, 0, true
  end
  target = next_kind and 1 or 0
  paint:invalidate()
end, {
  desc = "rain, snow, autumn leaves or spring petals over the conversation",
  args = { "rain", "snow", "autumn", "spring", "off" },
})
