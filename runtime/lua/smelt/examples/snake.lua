-- Snake game (demo of `smelt.paint` custom paint regions).
--
-- Not autoloaded — sits under `examples/` rather than `plugins/` so
-- nothing requires it at startup. To enable, drop the line
--
--     require('smelt.examples.snake')
--
-- into `~/.config/smelt/init.lua` (or your project's `.smelt/init.lua`).
-- That registers an `<F11>` global keybind that toggles the overlay,
-- plus a `:snake` command for non-keymap launches.
--
-- Why this exists: `smelt.paint.register(fn)` lets a Lua plugin paint
-- arbitrary cells into a leaf rect. Anything that doesn't fit the
-- editor's text + highlight model — sparklines, charts, retro game
-- grids — uses paint regions. Snake is a tight stress test: 24 × 24
-- logical cells via half-block compression (▀▄█), fixed tick, direct
-- cell writes via `slice:set / put_str / fill_rect`.
--
-- The half-block trick: each terminal row holds two logical rows
-- (`▀` paints the upper half, `▄` the lower, `█` both). With one
-- half-block per logical cell, vertical resolution doubles — so the
-- snake moves the same distance per tick whether it's heading
-- horizontally or vertically.

local M = {}

-- Logical game grid. Terminal rows = GRID_H / 2 (each row holds two
-- logical cells via the half-block trick). 24 × 24 fits inside the
-- default ScreenCenter overlay (70 % / 60 %) on terminals as small as
-- 80 × 24.
local GRID_W = 24
local GRID_H = 20 -- ⇒ 10 terminal rows after half-block packing
local TICK_MS = 100

-- Module-local state, `nil` when the overlay is closed. Mirrors
-- `perf_panel`'s `PANEL` pattern.
local STATE = nil

local function new_food(snake)
  for _ = 1, 200 do
    local r = math.random(0, GRID_H - 1)
    local c = math.random(0, GRID_W - 1)
    local clash = false
    for _, seg in ipairs(snake) do
      if seg.row == r and seg.col == c then clash = true; break end
    end
    if not clash then return { row = r, col = c } end
  end
  -- Worst-case fallback (board nearly full): drop on the first empty
  -- cell we can find rather than spinning forever.
  for r = 0, GRID_H - 1 do
    for c = 0, GRID_W - 1 do
      local clash = false
      for _, seg in ipairs(snake) do
        if seg.row == r and seg.col == c then clash = true; break end
      end
      if not clash then return { row = r, col = c } end
    end
  end
  return { row = 0, col = 0 }
end

local function init_state()
  local mid_row = math.floor(GRID_H / 2)
  local snake = {
    { row = mid_row, col = 5 },
    { row = mid_row, col = 6 },
    { row = mid_row, col = 7 }, -- head
  }
  return {
    snake = snake,
    direction = "right",
    next_direction = "right",
    food = new_food(snake),
    dead = false,
    score = 0,
    paint_id = nil,
    timer = nil,
    win = nil,
    buf = nil,
  }
end

local OPPOSITE = { up = "down", down = "up", left = "right", right = "left" }

local function turn(dir)
  return function()
    if not STATE or STATE.dead then return end
    if STATE.direction ~= OPPOSITE[dir] then
      STATE.next_direction = dir
    end
  end
end

local function tick()
  if not STATE or STATE.dead then return end
  STATE.direction = STATE.next_direction
  local head = STATE.snake[#STATE.snake]
  local dr, dc = 0, 0
  if STATE.direction == "up" then dr = -1
  elseif STATE.direction == "down" then dr = 1
  elseif STATE.direction == "left" then dc = -1
  elseif STATE.direction == "right" then dc = 1
  end
  local nr, nc = head.row + dr, head.col + dc

  -- Wall: die.
  if nr < 0 or nr >= GRID_H or nc < 0 or nc >= GRID_W then
    STATE.dead = true
    return
  end

  -- Self collision (skip the tail cell — it'll move out of the way
  -- this tick unless we're growing).
  for i = 2, #STATE.snake do
    local seg = STATE.snake[i]
    if seg.row == nr and seg.col == nc then
      STATE.dead = true
      return
    end
  end

  local ate = (nr == STATE.food.row and nc == STATE.food.col)
  table.insert(STATE.snake, { row = nr, col = nc })
  if ate then
    STATE.score = STATE.score + 1
    STATE.food = new_food(STATE.snake)
  else
    table.remove(STATE.snake, 1)
  end
end

-- Build a (row, col) → kind lookup so `paint` can answer
-- "what's at this cell" in O(1) per query rather than scanning the
-- snake body for each of GRID_W × GRID_H cells.
local function build_grid()
  local g = {}
  for i, seg in ipairs(STATE.snake) do
    local key = seg.row * 1000 + seg.col
    if i == #STATE.snake then
      g[key] = "head"
    else
      g[key] = "body"
    end
  end
  g[STATE.food.row * 1000 + STATE.food.col] = "food"
  return g
end

local COLORS = {
  body = "green",
  head = { fg = "green", bold = true }, -- bright green via bold
  food = "red",
}

local function color_for(kind)
  if kind == "head" then return "green" end -- head paints same hue as body
  if kind == "body" then return "green" end
  if kind == "food" then return "red" end
  return nil
end

-- Paint one terminal cell that represents two logical rows
-- (top = `2*ty`, bottom = `2*ty+1`). Picks the right half-block glyph
-- based on which halves are filled.
local function paint_pair(slice, term_y, term_x, top_kind, bot_kind)
  local top = top_kind ~= nil
  local bot = bot_kind ~= nil
  if not top and not bot then return end
  local top_color = color_for(top_kind)
  local bot_color = color_for(bot_kind)
  if top and bot then
    if top_color == bot_color then
      slice:set(term_y, term_x, "█", { fg = top_color })
    else
      -- ▀ paints upper half in fg, lower half in bg.
      slice:set(term_y, term_x, "▀", { fg = top_color, bg = bot_color })
    end
  elseif top then
    slice:set(term_y, term_x, "▀", { fg = top_color })
  else
    slice:set(term_y, term_x, "▄", { fg = bot_color })
  end
end

local function paint(slice, _ctx)
  if not STATE then return end
  local sw = slice:width()
  local sh = slice:height()
  local game_w = GRID_W
  local game_h_term = math.floor(GRID_H / 2) -- half-block packing

  -- Center the game rect inside the overlay's slice. If the slice is
  -- smaller than the game rect (very small terminal), clamp at 0 and
  -- let the rest clip naturally.
  local off_x = math.max(0, math.floor((sw - game_w) / 2))
  local off_y = math.max(0, math.floor((sh - game_h_term - 1) / 2)) -- -1 for status row

  -- Clear the playfield to a dark background so the snake's green
  -- half-blocks pop. Clearing the whole slice would wipe overlay
  -- chrome too, so we restrict to the game rect.
  slice:fill_rect(off_y, off_x, game_w, game_h_term, " ", { bg = "black" })

  local g = build_grid()
  for ty = 0, game_h_term - 1 do
    local top_row = 2 * ty
    local bot_row = 2 * ty + 1
    for x = 0, game_w - 1 do
      local top_kind = g[top_row * 1000 + x]
      local bot_kind = g[bot_row * 1000 + x]
      paint_pair(slice, off_y + ty, off_x + x, top_kind, bot_kind)
    end
  end

  -- Status row above the playfield.
  local status = string.format("score: %d", STATE.score)
  if STATE.dead then
    status = status .. "   GAME OVER (esc to close)"
  end
  local status_y = math.max(0, off_y - 1)
  slice:put_str(status_y, off_x, status, { fg = "white", bold = true })
end

local function close()
  if not STATE then return end
  if STATE.timer then smelt.timer.cancel(STATE.timer) end
  if STATE.paint_id then smelt.paint.unregister(STATE.paint_id) end
  if STATE.win then smelt.win.close(STATE.win) end
  STATE = nil
end

local function open()
  if STATE then return end
  math.randomseed(os.time())
  STATE = init_state()

  -- Hint strip at the bottom of the overlay carries focus so keymaps
  -- have a window to attach to. Buffer holds one line of help text;
  -- the paint region above does the actual game rendering.
  STATE.buf = smelt.buf.create()
  smelt.buf.set_lines(STATE.buf, {
    " hjkl / arrows: move    esc / ctrl-c: close ",
  })
  STATE.win = smelt.win.open(STATE.buf, { focusable = true })

  STATE.paint_id = smelt.paint.register(paint)

  smelt.win.set_keymap(STATE.win, "h", turn("left"))
  smelt.win.set_keymap(STATE.win, "j", turn("down"))
  smelt.win.set_keymap(STATE.win, "k", turn("up"))
  smelt.win.set_keymap(STATE.win, "l", turn("right"))
  smelt.win.set_keymap(STATE.win, "<Left>", turn("left"))
  smelt.win.set_keymap(STATE.win, "<Down>", turn("down"))
  smelt.win.set_keymap(STATE.win, "<Up>", turn("up"))
  smelt.win.set_keymap(STATE.win, "<Right>", turn("right"))
  smelt.win.set_keymap(STATE.win, "<Esc>", close)
  smelt.win.set_keymap(STATE.win, "<C-c>", close)

  smelt.ui.overlay.open({
    title = {
      { text = " snake ", fg = "green", bold = true },
      { text = "(F11 to close) ", fg = "grey", dim = true },
    },
    items = {
      { win = STATE.paint_id, height = "fill" },
      { win = STATE.win,      height = 1 },
    },
    modal = false,
    draggable = true,
    resizable = false,
  })
  smelt.win.set_focus(STATE.win)

  STATE.timer = smelt.timer.every(TICK_MS, tick)
end

local function toggle()
  if STATE then close() else open() end
end

smelt.cmd.register("snake", toggle)
smelt.keymap.set("", "<F11>", toggle)

return M
