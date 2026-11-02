-- /flappy: a bird through the pipes. smelt flaps until you press a key; then it is yours: space,
-- k or ↑ flaps. A pipe or the ground starts it over. q or Esc closes.
local W, H, GAP = 32, 13, 4
local bird, vy, pipes, score, t, human, ui

local function reset()
  bird, vy, pipes, score, t = H // 2, 0, {}, 0, 0
end

local function draw(s)
  for _, p in ipairs(pipes) do
    for y = 0, H - 1 do
      if (y < p[2] or y > p[2] + GAP) and p[1] >= 0 then s:put_str(math.floor(p[1]), y, "██", { fg = { 110, 200, 80 } }) end
    end
  end
  s:put_str(5, math.floor(bird), t % 4 < 2 and "▀●" or "▄●", { fg = { 255, 210, 60 }, bold = true })
  s:put_str(W - 4, 0, string.format("%3d", score), { fg = { 255, 255, 255 }, bold = true })
end

local function flap() vy = -1 end

local function tick()
  t = t + 1
  if t % 14 == 0 then pipes[#pipes + 1] = { W - 2, math.random(1, H - GAP - 2) } end
  local ahead
  for i = #pipes, 1, -1 do
    local p = pipes[i]
    p[1] = p[1] - 0.5
    if p[1] < -2 then
      table.remove(pipes, i)
    elseif p[1] + 2 >= 6 and (not ahead or p[1] < ahead[1]) then
      ahead = p
    end
    if math.floor(p[1]) == 3 and math.floor(p[1] + 0.5) == 4 then score = score + 1 end
  end
  vy = vy + 0.25
  -- smelt flaps whenever the bird would sink below the middle of the next gap.
  if not human and bird + vy > (ahead and ahead[2] + GAP - 1 or H // 2) then flap() end
  bird = bird + vy
  local y = math.floor(bird)
  if y < 0 or y >= H then return reset() end
  for _, p in ipairs(pipes) do
    local x = math.floor(p[1])
    if x <= 6 and x + 1 >= 5 and (y < p[2] or y > p[2] + GAP) then return reset() end
  end
end

local function play()
  human = true
  flap()
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
    name = "flappy", title = " flappy · space ", anchor = "screen_at", corner = "ne", row = 1, col = 1,
    width = W + 2, height = H + 2, modal = false,
    -- The empty window holds focus so the keymaps fire; the game is a paint.
    layout = layout.vbox({
      { layout.leaf(ui.win), height = 0 },
      { layout.leaf(ui.paint, { measure = { W, H } }), height = "fit" },
    }),
    keymaps = {
      { key = "<Space>", on_press = play }, { key = "k", on_press = play }, { key = "<Up>", on_press = play },
      { key = "q", on_press = close }, { key = "<Esc>", on_press = close }, { key = "<C-c>", on_press = close },
    },
  })
  ui.win:focus()
  ui.timer = smelt.timer.every(70, tick)
end

smelt.cmd.register("flappy", function()
  if ui then close() else open() end
end, { desc = "play flappy bird" })
