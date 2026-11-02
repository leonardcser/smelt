-- /breakout: bricks, a ball and a paddle. smelt plays until you press a key; then the paddle is
-- yours: h or ← left, l or → right. Three balls a game. q or Esc closes.
local W, H, PAD = 30, 12, 5
local COLORS = { { 240, 90, 90 }, { 245, 170, 70 }, { 240, 220, 90 }, { 110, 210, 120 } }
local bricks, ball, dx, dy, pad, balls, human, ui

local function serve()
  ball, dx, dy = { pad + PAD // 2, H - 3 }, math.random() < 0.5 and -1 or 1, -1
end

local function reset()
  bricks, pad, balls = {}, (W - PAD) // 2, 3
  for row = 1, 4 do for x = 0, W - 1, 3 do bricks[row * 100 + x // 3] = true end end
  serve()
end

local function draw(s)
  for key in pairs(bricks) do
    local row, i = key // 100, key % 100
    s:fill(i * 3, row, 2, 1, " ", { bg = COLORS[row] })
  end
  s:put_str(pad, H - 1, ("▀"):rep(PAD), { fg = { 200, 200, 220 } })
  s:put_str(W - balls, 0, ("●"):rep(balls), { fg = { 120, 116, 112 } })
  s:set(ball[1], ball[2], "●", { fg = { 255, 255, 255 } })
end

local function tick()
  if not human then pad = math.max(0, math.min(W - PAD, ball[1] - PAD // 2)) end
  local x, y = ball[1] + dx, ball[2] + dy
  if x < 0 or x >= W then dx, x = -dx, ball[1] - dx end
  if y < 0 then dy, y = -dy, ball[2] - dy end
  if y == H - 1 then
    if x >= pad - 1 and x <= pad + PAD then
      -- The paddle's ends send the ball back the way it came.
      if x < pad + 1 then dx = -1 elseif x >= pad + PAD - 1 then dx = 1 end
      dy, y = -1, ball[2] - 1
    else
      balls = balls - 1
      if balls == 0 then reset() else serve() end
      return
    end
  end
  local key = y * 100 + x // 3
  if bricks[key] then bricks[key], dy, y = nil, -dy, ball[2] end
  ball = { x, y }
  if next(bricks) == nil then reset() end
end

local function move(by)
  return function()
    human = true
    pad = math.max(0, math.min(W - PAD, pad + by))
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
  human = false
  reset()
  ui = { win = smelt.win.new(smelt.buf.new(), { surface = "readonly_text", hide_cursor = true }) }
  ui.paint = smelt.paint.register(draw)
  local layout = smelt.ui.layout
  smelt.overlay.new({
    name = "breakout", title = " breakout · ←→ ", anchor = "screen_at", corner = "ne", row = 1, col = 1,
    width = W + 2, height = H + 2, modal = false,
    -- The empty window holds focus so the keymaps fire; the game is a paint.
    layout = layout.vbox({
      { layout.leaf(ui.win), height = 0 },
      { layout.leaf(ui.paint, { measure = { W, H } }), height = "fit" },
    }),
    keymaps = {
      { key = "h", on_press = move(-2) }, { key = "<Left>", on_press = move(-2) },
      { key = "l", on_press = move(2) }, { key = "<Right>", on_press = move(2) },
      { key = "q", on_press = close }, { key = "<Esc>", on_press = close }, { key = "<C-c>", on_press = close },
    },
  })
  ui.win:focus()
  ui.timer = smelt.timer.every(60, tick)
end

smelt.cmd.register("breakout", function()
  if ui then close() else open() end
end, { desc = "play breakout" })
