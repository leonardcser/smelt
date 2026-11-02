-- /invaders: the aliens march and the ship fights back. smelt flies it until you press a key; then
-- it is yours: h or ← left, l or → right, space fires. q or Esc closes.
local W, H = 30, 12
local COLORS = { { 120, 255, 140 }, { 120, 200, 255 }, { 255, 130, 220 } }
local aliens, ox, oy, dir, ship, shots, t, human, ui

local function reset()
  aliens, ox, oy, dir, ship, shots, t = {}, 2, 1, 1, 15, {}, 0
  for r = 0, 2 do for c = 0, 5 do aliens[r * 10 + c] = true end end
end

local function draw(s)
  for key in pairs(aliens) do
    local r, c = key // 10, key % 10
    s:put_str(ox + c * 4, oy + r * 2, t % 8 < 4 and "▚▞" or "▞▚", { fg = COLORS[r + 1] })
  end
  for _, p in ipairs(shots) do s:set(p[1], p[2], "│", { fg = { 255, 240, 120 } }) end
  s:put_str(ship - 1, H - 1, "▄█▄", { fg = { 230, 230, 240 } })
end

local function fire()
  if #shots < 3 then shots[#shots + 1] = { ship, H - 2 } end
end

local function tick()
  t = t + 1
  if t % 4 == 0 then
    ox = ox + dir
    if ox <= 0 or ox >= W - 23 then dir, oy = -dir, oy + 1 end
  end
  if not human then
    local target = next(aliens)
    if target then ship = ship + ((ox + target % 10 * 4) > ship and 1 or -1) end
    if t % 5 == 0 then fire() end
  end
  for i = #shots, 1, -1 do
    local p = shots[i]
    p[2] = p[2] - 1
    local c, r = (p[1] - ox) // 4, (p[2] - oy) // 2
    if (p[1] - ox) % 4 < 2 and (p[2] - oy) % 2 == 0 and aliens[r * 10 + c] then
      aliens[r * 10 + c] = nil
      table.remove(shots, i)
    elseif p[2] < 0 then
      table.remove(shots, i)
    end
  end
  if next(aliens) == nil or oy > 6 then reset() end
end

local function play(fn)
  return function()
    human = true
    fn()
  end
end

local function move(by)
  return play(function() ship = math.max(1, math.min(W - 2, ship + by)) end)
end

local function close()
  if not ui then return end
  ui.timer:remove()
  ui.paint:remove()
  ui.win:close()
  ui = nil
end

local function open()
  human = false
  reset()
  ui = { win = smelt.win.new(smelt.buf.new(), { surface = "readonly_text", hide_cursor = true }) }
  ui.paint = smelt.paint.register(draw)
  local layout = smelt.ui.layout
  smelt.overlay.new({
    name = "invaders", title = " invaders · ←→ space ", anchor = "screen_at", corner = "ne", row = 1, col = 1,
    width = W + 2, height = H + 2, modal = false,
    -- The empty window holds focus so the keymaps fire; the game is a paint.
    layout = layout.vbox({
      { layout.leaf(ui.win), height = 0 },
      { layout.leaf(ui.paint, { measure = { W, H } }), height = "fit" },
    }),
    keymaps = {
      { key = "h", on_press = move(-1) }, { key = "<Left>", on_press = move(-1) },
      { key = "l", on_press = move(1) }, { key = "<Right>", on_press = move(1) },
      { key = "<Space>", on_press = play(fire) },
      { key = "q", on_press = close }, { key = "<Esc>", on_press = close }, { key = "<C-c>", on_press = close },
    },
  })
  ui.win:focus()
  ui.timer = smelt.timer.every(80, tick)
end

smelt.cmd.register("invaders", function()
  if ui then close() else open() end
end, { desc = "play space invaders" })
