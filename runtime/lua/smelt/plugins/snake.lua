-- /snake (or F11) - snake in a floating window. hjkl or the arrows steer,
-- space starts over, q, Esc or Ctrl-C closes.
-- The title shows the score and the best one this session.
--
-- Two board rows share one terminal row: `▀` paints the top half in its
-- foreground and the bottom half in its background.

local W, H = 32, 32
local TICK_MS = 100
local FOOD = { rgb = { 240, 80, 60 } }
local STEP = { up = { 0, -1 }, down = { 0, 1 }, left = { -1, 0 }, right = { 1, 0 } }
local OPPOSITE = { up = "down", down = "up", left = "right", right = "left" }

local session = smelt.state.get("smelt.snake")
local game, ui, show

local function key(x, y) return y * W + x end

-- A random free cell, or nil once the snake fills the board.
local function place_food(snake)
  local taken, free = {}, {}
  for _, p in ipairs(snake) do taken[key(p[1], p[2])] = true end
  for i = 0, W * H - 1 do
    if not taken[i] then free[#free + 1] = i end
  end
  if #free == 0 then return nil end
  local i = free[math.random(#free)]
  return { i % W, i // W }
end

local function new_game()
  local y = H // 2
  local snake = { { 7, y }, { 6, y }, { 5, y } } -- head first
  game = { snake = snake, dir = "right", next_dir = "right", food = place_food(snake), score = 0, dead = false }
end

local function tick()
  if game.dead then return end
  game.dir = game.next_dir
  local head, d = game.snake[1], STEP[game.dir]
  local x, y = head[1] + d[1], head[2] + d[2]
  local hit = x < 0 or y < 0 or x >= W or y >= H
  -- The tail moves out of the way this tick, so it is not an obstacle.
  for i = 1, #game.snake - 1 do
    local p = game.snake[i]
    if p[1] == x and p[2] == y then hit = true end
  end
  if hit then
    game.dead = true
    return show()
  end
  table.insert(game.snake, 1, { x, y })
  if game.food and x == game.food[1] and y == game.food[2] then
    game.score = game.score + 1
    if game.score > (session.best or 0) then session.best = game.score end
    game.food = place_food(game.snake)
    show()
  else
    table.remove(game.snake)
  end
end

-- The head is bright and the body fades toward the tail.
local function shade(i)
  local f = 1 - (i - 1) / math.max(#game.snake, 8) * 0.7
  return { rgb = { math.floor(120 * f), math.floor(220 * f), math.floor(90 * f) } }
end

local function paint(slice)
  if not game then return end
  local bg = smelt.theme.get("SmeltScrollPillBg").bg
  local cells = {}
  for i, p in ipairs(game.snake) do cells[key(p[1], p[2])] = shade(i) end
  if game.food then cells[key(game.food[1], game.food[2])] = FOOD end
  local cols, rows = math.min(W, slice:width()), math.min(H // 2, slice:height())
  slice:fill_rect(0, 0, cols, rows, " ", { bg = bg })
  for row = 0, rows - 1 do
    for x = 0, cols - 1 do
      local top, bottom = cells[key(x, row * 2)], cells[key(x, row * 2 + 1)]
      if top or bottom then slice:set(row, x, "▀", { fg = top or bg, bg = bottom or bg }) end
    end
  end
end

local function turn(dir)
  return function()
    if not game.dead and game.dir ~= OPPOSITE[dir] then game.next_dir = dir end
  end
end

local function restart()
  new_game()
  show()
end

local function close()
  if not ui then return end
  ui.timer:remove()
  ui.paint:remove()
  ui.win:close()
  ui, game = nil, nil
end

-- Re-opening the named overlay refreshes its title with the score.
show = function()
  local title = (" snake %d · best %d "):format(game.score, session.best or 0)
  if game.dead then title = title .. "· game over " end
  local layout = smelt.ui.layout
  smelt.overlay.new({
    name = "smelt.snake",
    title = title,
    border = { all = "Comment" },
    anchor = "screen_at", corner = "ne", row = 1, col = 0,
    width = W + 2, height = H // 2 + 2,
    -- The empty window holds focus so the keymaps fire; the board is a paint.
    layout = layout.vbox({
      { layout.leaf(ui.win), height = 0 },
      { layout.leaf(ui.paint, { measure = { W, H // 2 } }), height = "fit" },
    }),
    modal = false,
    draggable = true,
    resizable = false,
    keymaps = {
      { key = "h", on_press = turn("left") },
      { key = "j", on_press = turn("down") },
      { key = "k", on_press = turn("up") },
      { key = "l", on_press = turn("right") },
      { key = "<Left>", on_press = turn("left") },
      { key = "<Down>", on_press = turn("down") },
      { key = "<Up>", on_press = turn("up") },
      { key = "<Right>", on_press = turn("right") },
      { key = "<Space>", on_press = restart },
      { key = "q", on_press = close },
      { key = "<Esc>", on_press = close },
      { key = "<C-c>", on_press = close },
    },
  })
end

local function open()
  math.randomseed(os.time())
  new_game()
  ui = { win = smelt.win.new(smelt.buf.new(), { surface = "readonly_text", hide_cursor = true }) }
  ui.paint = smelt.paint.register(paint, { name = "smelt.snake.paint" })
  show()
  ui.win:focus()
  ui.timer = smelt.timer.every(TICK_MS, tick)
end

local function toggle()
  if ui then close() else open() end
end

smelt.cmd.register("snake", toggle, { desc = "play snake" })
smelt.keymap.set("", "<F11>", toggle)
