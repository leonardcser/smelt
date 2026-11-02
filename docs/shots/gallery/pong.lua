-- /pong: you against smelt. smelt plays both paddles until you press a key; then the left one is
-- yours: k or ↑ up, j or ↓ down. q or Esc closes.
local W, H = 30, 11
local ball, dx, dy, left, right, score, human, ui

local function reset()
  ball, dx, dy, left, right, score, human = { W // 2, H // 2 }, 1, 1, 4, 4, { 0, 0 }, false
end

local function draw(s)
  for y = 0, H - 1, 2 do s:set(W // 2, y, "┊", { fg = { 90, 84, 80 } }) end
  s:put_str(W // 2 - 3, 0, tostring(score[1]), { bold = true })
  s:put_str(W // 2 + 2, 0, tostring(score[2]), { bold = true })
  for i = 0, 2 do
    s:set(0, left + i, "█", { fg = { 120, 200, 255 } })
    s:set(W - 1, right + i, "█", { fg = { 255, 120, 160 } })
  end
  s:set(ball[1], ball[2], "●", { fg = { 255, 210, 90 } })
end

-- smelt's paddle follows the ball while it comes its way, and misses now and then.
local function track(p, going)
  if going and math.random() < 0.8 then p = p + (ball[2] > p + 1 and 1 or ball[2] < p + 1 and -1 or 0) end
  return math.max(0, math.min(H - 3, p))
end

local function tick()
  ball[1], ball[2] = ball[1] + dx, ball[2] + dy
  if ball[2] <= 0 or ball[2] >= H - 1 then dy = -dy end
  if not human then left = track(left, dx < 0) end
  right = track(right, dx > 0)
  if ball[1] == 1 and ball[2] >= left and ball[2] <= left + 2 then dx = 1 end
  if ball[1] == W - 2 and ball[2] >= right and ball[2] <= right + 2 then dx = -1 end
  if ball[1] <= 0 or ball[1] >= W - 1 then
    local side = ball[1] <= 0 and 2 or 1
    score[side] = score[side] + 1
    ball, dx = { W // 2, math.random(2, H - 3) }, -dx
  end
end

local function move(by)
  return function()
    human = true
    left = math.max(0, math.min(H - 3, left + by))
  end
end

local function close()
  if not ui then return end
  ui.timer:remove()
  ui.paint:remove()
  ui.win:close()
  ui = nil
end

local function open()
  reset()
  ui = { win = smelt.win.new(smelt.buf.new(), { surface = "readonly_text", hide_cursor = true }) }
  ui.paint = smelt.paint.register(draw)
  local layout = smelt.ui.layout
  smelt.overlay.new({
    name = "pong", title = " pong · ↑↓ ", anchor = "screen_at", corner = "ne", row = 1, col = 1,
    width = W + 2, height = H + 2, modal = false,
    -- The empty window holds focus so the keymaps fire; the game is a paint.
    layout = layout.vbox({
      { layout.leaf(ui.win), height = 0 },
      { layout.leaf(ui.paint, { measure = { W, H } }), height = "fit" },
    }),
    keymaps = {
      { key = "k", on_press = move(-1) }, { key = "<Up>", on_press = move(-1) },
      { key = "j", on_press = move(1) }, { key = "<Down>", on_press = move(1) },
      { key = "q", on_press = close }, { key = "<Esc>", on_press = close }, { key = "<C-c>", on_press = close },
    },
  })
  ui.win:focus()
  ui.timer = smelt.timer.every(70, tick)
end

smelt.cmd.register("pong", function()
  if ui then close() else open() end
end, { desc = "play pong against smelt" })
