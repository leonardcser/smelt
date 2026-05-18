-- /banner — interactive picker for the smelt logo art variants.
--
-- Demo of `smelt.overlay` + `smelt.timer` + `smelt.paint` reusing canonical
-- pixel data from `smelt.banner` (LOGO_PIXELS, WORDMARK_PIXELS,
-- MINI_FIRE_FRAMES, PALETTE). Tab cycles variants; Space toggles the
-- animation loop; Esc / Ctrl-C closes.
--
-- Not autoloaded. Add `require("smelt.examples.banner_picker")` to
-- `init.lua` to wire the `/banner` command.
--
-- The overlay is sized to its content via a `smelt.overlay.layout.measure(...)`
-- handle on the paint leaf, updated on Tab to the current variant's pixel
-- size. The layout's natural size feeds the overlay's rect every frame,
-- so cycling variants resizes smoothly with no close+reopen churn.
--
-- Keys live at overlay scope via `opts.keymaps`, so a single registration
-- covers the whole picker. A zero-height invisible window anchors focus
-- inside the overlay (the cascade routes overlay keymaps via whichever
-- leaf the focused window belongs to).
--
-- Hot-reload survival: variant / loop / frame / is_open live in
-- `smelt.state("smelt.banner_picker")`, which outlives `/reload`. The
-- overlay, window, buffer, and paint slot all carry `name = "..."` so
-- their backing structures survive reload — the closures get atomically
-- swapped to the freshly-loaded versions. On every Lua-context bring-up
-- the bottom of this file checks `persist().is_open` and re-opens the
-- picker if it was open at /reload time, so editing this file refreshes
-- the sprites without closing the picker.

local banner = require("smelt.banner")

local M = {}

-- Lazy-defaulted accessor for reload-surviving state.
local function persist()
	local s = smelt.state("smelt.banner_picker")
	if s.variant_idx == nil then
		s.variant_idx = 1
	end
	if s.looping == nil then
		s.looping = false
	end
	if s.frame == nil then
		s.frame = 0
	end
	if s.is_open == nil then
		s.is_open = false
	end
	return s
end

local STATE = nil

local LABELS = {
	"full",
	"full+version",
	"icon",
	"wordmark+version",
	"wordmark+fire",
}

local TICK_MS = 80

local function compose_fire_wordmark(fire, wordmark)
	local fire_w, wm_w = #fire[1], #wordmark[1]
	local width = math.max(fire_w, wm_w)
	local function centered(row, row_w)
		local left = math.floor((width - row_w) / 2)
		return string.rep(".", left) .. row .. string.rep(".", width - left - row_w)
	end
	local rows = {}
	for _, r in ipairs(fire) do
		rows[#rows + 1] = centered(r, fire_w)
	end
	for _, r in ipairs(wordmark) do
		rows[#rows + 1] = centered(r, wm_w)
	end
	-- Pad at top so the wordmark sits flush against the version line below.
	if #rows % 2 == 1 then
		table.insert(rows, 1, string.rep(".", width))
	end
	return rows
end

-- Returns (cell_width, cell_height, paint_fn(slice, row0, col0)) for the
-- given variant at the given animation frame.
local function variant_info(idx, frame_idx)
	local label = LABELS[idx]
	local version = "v" .. (smelt.version or "")
	if label == "full" then
		local rows = banner.compose(banner.LOGO_PIXELS, banner.WORDMARK_PIXELS)
		return #rows[1], math.ceil(#rows / 2), function(slice, r, c)
			banner.paint_pixels(slice, r, c, rows)
		end
	elseif label == "full+version" then
		local rows, vcol, vrow = banner.compose(banner.LOGO_PIXELS, banner.WORDMARK_PIXELS)
		return #rows[1],
			math.ceil(#rows / 2),
			function(slice, r, c)
				banner.paint_pixels(slice, r, c, rows)
				slice:put_str(r + vrow, c + vcol, version, { dim = true })
			end
	elseif label == "icon" then
		local rows = banner.LOGO_PIXELS
		return #rows[1], math.ceil(#rows / 2), function(slice, r, c)
			banner.paint_pixels(slice, r, c, rows)
		end
	elseif label == "wordmark+version" then
		local rows = banner.WORDMARK_PIXELS
		local w, h = #rows[1], math.ceil(#rows / 2)
		return w,
			h + 1,
			function(slice, r, c)
				banner.paint_pixels(slice, r, c, rows)
				local pad = math.floor((w - #version) / 2)
				slice:put_str(r + h, c + pad, version, { dim = true })
			end
	else -- "wordmark+fire"
		local frames = banner.MINI_FIRE_FRAMES
		local fire = frames[(frame_idx % #frames) + 1]
		local rows = compose_fire_wordmark(fire, banner.WORDMARK_PIXELS)
		local w, h = #rows[1], math.ceil(#rows / 2)
		return w,
			h + 1,
			function(slice, r, c)
				banner.paint_pixels(slice, r, c, rows)
				local pad = math.floor((w - #version) / 2)
				slice:put_str(r + h, c + pad, version, { dim = true })
			end
	end
end

local function paint(slice, _ctx)
	if not STATE then
		return
	end
	local w, h, paint_fn = variant_info(persist().variant_idx, persist().frame)
	local sw = slice:width()
	local sh = slice:height()
	local r0 = math.max(0, math.floor((sh - h) / 2))
	local c0 = math.max(0, math.floor((sw - w) / 2))
	paint_fn(slice, r0, c0)
end

local function update_measure()
	if not STATE then
		return
	end
	local w, h = variant_info(persist().variant_idx, persist().frame)
	STATE.measure:set(w, h)
end

local function close()
	if not STATE then
		return
	end
	if STATE.timer then
		STATE.timer:remove()
	end
	if STATE.paint then
		STATE.paint:remove()
	end
	if STATE.win then
		STATE.win:close()
	end
	if STATE.overlay then
		STATE.overlay:close()
	end
	STATE = nil
	persist().is_open = false
end

local function cycle(delta)
	persist().variant_idx = ((persist().variant_idx - 1 + delta) % #LABELS) + 1
	persist().frame = 0
	update_measure()
end

local function open()
	if STATE then
		return
	end
	STATE = {}

	local w, h = variant_info(persist().variant_idx, persist().frame)
	STATE.measure = smelt.overlay.layout.measure(w, h)

	-- Invisible zero-height window pinned inside the overlay layout. Its
	-- only job is to anchor focus so the cascade can route overlay-scoped
	-- keymaps through `overlay_for_leaf(focused_win)`. No buffer content,
	-- no per-window key bindings.
	STATE.buf = smelt.buf.new({ name = "smelt.banner_picker.focus.buf" })
	STATE.win = smelt.win.new(STATE.buf, {
		name = "smelt.banner_picker.focus.win",
		focusable = true,
	})

	STATE.paint = smelt.paint.register(paint, { name = "smelt.banner_picker.paint" })

	STATE.overlay = smelt.overlay.new({
		name = "smelt.banner_picker",
		title = {
			{ text = " /banner ", dim = true, bold = true },
		},
		anchor = "center",
		border = { all = "Comment" },
		layout = smelt.overlay.layout.vbox({
			{
				smelt.overlay.layout.leaf(STATE.paint, { measure = STATE.measure }),
				height = "fit",
			},
			{
				smelt.overlay.layout.leaf(STATE.win, { measure = { 0, 0 } }),
				height = 0,
			},
		}, { padding = 2 }),
		modal = false,
		draggable = true,
		resizable = false,
		keymaps = {
			{ key = "<Tab>", on_press = function() cycle(1) end },
			{ key = "<S-Tab>", on_press = function() cycle(-1) end },
			{ key = "<Space>", on_press = function() persist().looping = not persist().looping end },
			{ key = "<Esc>", on_press = close },
			{ key = "<C-c>", on_press = close },
		},
	})
	STATE.win:focus()

	STATE.timer = smelt.timer.every(TICK_MS, function()
		if persist().looping then
			persist().frame = persist().frame + 1
		end
	end)

	persist().is_open = true
end

local function toggle()
	if STATE then
		close()
	else
		open()
	end
end

smelt.cmd.register("banner", toggle, { desc = "logo variant picker (demo)" })

-- Module body re-runs with the host pointer live on every Lua-context
-- bring-up (cold start and `/reload`). On the first cold-start
-- `persist().is_open` is false so this is a no-op; after `/reload` it
-- re-opens the picker on top of the surviving named overlay / paint
-- slot so the sprite edit shows up in place.
if persist().is_open then
	open()
end

return M
