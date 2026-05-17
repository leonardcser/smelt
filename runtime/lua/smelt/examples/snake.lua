-- Snake game — demo of `smelt.paint` custom paint regions.
-- Not autoloaded; add `require("smelt.examples.snake")` to init.lua.
-- Registers F11 to toggle and a /snake command.
--
-- Half-block trick: `▀` paints the upper half-cell, `▄` the lower, `█` both.
-- Two logical rows pack into one terminal row, doubling vertical resolution.

local M = {}

local GRID_W = 32
local GRID_H = 32 -- 16 terminal rows after half-block packing
local TICK_MS = 100

-- Overlay: status row + game (GRID_H/2 term rows) + hint strip + 1-cell border each side.
local OVERLAY_W = GRID_W + 2
local OVERLAY_H = (GRID_H / 2) + 1 + 1 + 2

local BG = { r = 18, g = 22, b = 30 } -- dark playfield so snake's green pops

local STATE = nil

local function new_food(snake)
	for _ = 1, 200 do
		local r = math.random(0, GRID_H - 1)
		local c = math.random(0, GRID_W - 1)
		local clash = false
		for _, seg in ipairs(snake) do
			if seg.row == r and seg.col == c then
				clash = true
				break
			end
		end
		if not clash then
			return { row = r, col = c }
		end
	end
	-- Board nearly full fallback: scan for the first empty cell.
	for r = 0, GRID_H - 1 do
		for c = 0, GRID_W - 1 do
			local clash = false
			for _, seg in ipairs(snake) do
				if seg.row == r and seg.col == c then
					clash = true
					break
				end
			end
			if not clash then
				return { row = r, col = c }
			end
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
		if not STATE or STATE.dead then
			return
		end
		if STATE.direction ~= OPPOSITE[dir] then
			STATE.next_direction = dir
		end
	end
end

local function tick()
	if not STATE or STATE.dead then
		return
	end
	STATE.direction = STATE.next_direction
	local head = STATE.snake[#STATE.snake]
	local dr, dc = 0, 0
	if STATE.direction == "up" then
		dr = -1
	elseif STATE.direction == "down" then
		dr = 1
	elseif STATE.direction == "left" then
		dc = -1
	elseif STATE.direction == "right" then
		dc = 1
	end
	local nr, nc = head.row + dr, head.col + dc

	-- Wall: die.
	if nr < 0 or nr >= GRID_H or nc < 0 or nc >= GRID_W then
		STATE.dead = true
		return
	end

	-- Skip tail cell: it moves out of the way this tick unless we're growing.
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

-- O(1) cell-kind lookup for the paint pass.
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

local function color_for(kind)
	if kind == "head" then
		return "green"
	end -- head paints same hue as body
	if kind == "body" then
		return "green"
	end
	if kind == "food" then
		return "red"
	end
	return nil
end

-- Paint one terminal cell for two logical rows (top=2*ty, bottom=2*ty+1).
-- Explicit bg on the empty half prevents the terminal default from showing through.
local function paint_pair(slice, term_y, term_x, top_kind, bot_kind, bg)
	local top = top_kind ~= nil
	local bot = bot_kind ~= nil
	if not top and not bot then
		return
	end
	local top_color = color_for(top_kind)
	local bot_color = color_for(bot_kind)
	if top and bot then
		if top_color == bot_color then
			slice:set(term_y, term_x, "█", { fg = top_color })
		else
			slice:set(term_y, term_x, "▀", { fg = top_color, bg = bot_color })
		end
	elseif top then
		slice:set(term_y, term_x, "▀", { fg = top_color, bg = bg })
	else
		slice:set(term_y, term_x, "▄", { fg = bot_color, bg = bg })
	end
end

local function paint(slice, _ctx)
	if not STATE then
		return
	end
	local sw = slice:width()
	local sh = slice:height()
	local game_w = math.min(GRID_W, sw)
	local game_h_term = math.min(math.floor(GRID_H / 2), math.max(0, sh - 1))

	local status_y = 0
	local off_y = 1
	local off_x = 0

	slice:fill_rect(off_y, off_x, game_w, game_h_term, " ", { bg = BG })

	local g = build_grid()
	for ty = 0, game_h_term - 1 do
		local top_row = 2 * ty
		local bot_row = 2 * ty + 1
		for x = 0, game_w - 1 do
			local top_kind = g[top_row * 1000 + x]
			local bot_kind = g[bot_row * 1000 + x]
			paint_pair(slice, off_y + ty, off_x + x, top_kind, bot_kind, BG)
		end
	end

	local status = string.format(" score: %d", STATE.score)
	if STATE.dead then
		status = status .. "   GAME OVER"
	end
	status = status .. string.rep(" ", math.max(0, sw - #status)) -- pad to erase stale chrome

	slice:put_str(status_y, 0, status, { fg = "white", bold = true })
end

local function reset()
	if not STATE then
		return
	end
	math.randomseed(os.time())
	local fresh = init_state()
	STATE.snake = fresh.snake
	STATE.direction = fresh.direction
	STATE.next_direction = fresh.next_direction
	STATE.food = fresh.food
	STATE.dead = false
	STATE.score = 0
end

local function close()
	if not STATE then
		return
	end
	if STATE.timer then
		smelt.timer.cancel(STATE.timer)
	end
	if STATE.paint_id then
		smelt.paint.unregister(STATE.paint_id)
	end
	if STATE.win then
		STATE.win:close()
	end
	STATE = nil
end

local function open()
	if STATE then
		return
	end
	math.randomseed(os.time())
	STATE = init_state()

	STATE.buf = smelt.buf.new()
	STATE.buf:lines({
		" hjkl / arrows: move    space: reset    esc / ctrl-c: close ",
	})
	STATE.win = smelt.win.new(STATE.buf, { focusable = true })

	STATE.paint_id = smelt.paint.register(paint)

	STATE.win:key("h", turn("left"))
	STATE.win:key("j", turn("down"))
	STATE.win:key("k", turn("up"))
	STATE.win:key("l", turn("right"))
	STATE.win:key("<Left>", turn("left"))
	STATE.win:key("<Down>", turn("down"))
	STATE.win:key("<Up>", turn("up"))
	STATE.win:key("<Right>", turn("right"))
	STATE.win:key("<Space>", reset)
	STATE.win:key("<Esc>", close)
	STATE.win:key("<C-c>", close)

	smelt.overlay.new({
		title = {
			{ text = " snake ", fg = "green", bold = true },
			{ text = "(F11 to close) ", fg = "grey", dim = true },
		},
		width = OVERLAY_W,
		height = OVERLAY_H,
		layout = smelt.ui.layout.vbox({
			{ smelt.ui.layout.leaf(STATE.paint_id), height = "fill" },
			{ smelt.ui.layout.leaf(STATE.win),      height = 1      },
		}),
		modal = false,
		draggable = true,
		resizable = false,
	})
	STATE.win:focus()

	STATE.timer = smelt.timer.every(TICK_MS, tick)
end

local function toggle()
	if STATE then
		close()
	else
		open()
	end
end

smelt.cmd.register("snake", toggle, { desc = "snake game (demo)" })
smelt.keymap.set("", "<F11>", toggle)

return M
