-- Empty-state logo overlay + shutdown logo/resume-hint banner.
--
-- The splash is a non-focusable overlay centered over the transcript on
-- zero-message sessions; it tears down on the first turn. On clean shutdown
-- the same logo + dimmed version + resume hint print to the scrollback.
--
-- The version label is a real buffer so users can select / copy it. Art
-- lives in `smelt.banner` — override `FIRE_PIXELS` / `WORDMARK_PIXELS` /
-- `PALETTE` to retheme, or disable this module via
-- `smelt.builtins.disable({ plugins = { "banner" } })` in `early.lua`.

local banner = require("smelt.banner")

local state = {
	overlay = nil,
	paint = nil,
	version_buf = nil,
	version_win = nil,
	timer = nil,
	sim = nil,
	fire_pixels = nil,
	held = false,
}

local FIRE_HEADROOM = 4
local SIM_TOP_PAD = FIRE_HEADROOM * 2
local SIM_RAMP_TICKS = 10
local SIM_COOLDOWN_TICKS = 8
local SIM_DELAY_MS = 52

local HEAT = { R = 0.28, O = 0.48, o = 0.66, Y = 0.9 }

local function heat_for(ch)
	return HEAT[ch] or 0
end

local function char_for_heat(v)
	if v > 0.8 then
		return "Y"
	end
	if v > 0.6 then
		return "o"
	end
	if v > 0.38 then
		return "O"
	end
	if v > 0.18 then
		return "R"
	end
	return "."
end

local function fire_width()
	return #banner.FIRE_PIXELS[1]
end

local function fire_height()
	return SIM_TOP_PAD + #banner.FIRE_PIXELS
end

local function canonical_row(y)
	local row = banner.FIRE_PIXELS[y - SIM_TOP_PAD]
	if row then
		return row
	end
	return string.rep(".", fire_width())
end

local function canonical_heat()
	local heat = {}
	for y = 1, fire_height() do
		local row = canonical_row(y)
		heat[y] = {}
		for x = 1, #row do
			heat[y][x] = heat_for(row:sub(x, x))
		end
	end
	return heat
end

local function seeded_sim()
	return {
		tick = 0,
		cooldown = 0,
		heat = canonical_heat(),
		canonical = canonical_heat(),
	}
end

local function add_source(grid, x, y, strength, radius_x, radius_y)
	for yy = math.max(1, y - radius_y), math.min(#grid, y + radius_y) do
		for xx = math.max(1, x - radius_x), math.min(#grid[yy], x + radius_x) do
			local dx = math.abs(xx - x) / math.max(1, radius_x)
			local dy = math.abs(yy - y) / math.max(1, radius_y)
			local falloff = math.max(0, 1 - (dx * 0.72 + dy * 0.95))
			grid[yy][xx] = math.min(1, grid[yy][xx] + strength * falloff)
		end
	end
end

local function overlay_ember(rows, tick)
	local ember
	if tick >= 4 and tick <= 6 then
		ember = { y = SIM_TOP_PAD + 4, x = 15, ch = "Y" }
	elseif tick <= 9 then
		ember = { y = SIM_TOP_PAD + 3, x = 15, ch = "o" }
	elseif tick <= 12 then
		ember = { y = SIM_TOP_PAD + 3, x = 16, ch = "O" }
	elseif tick <= 15 then
		ember = { y = SIM_TOP_PAD + 2, x = 17, ch = "R" }
	elseif tick <= 17 then
		ember = { y = SIM_TOP_PAD + 1, x = 18, ch = "R" }
	end
	if not ember then
		return
	end
	local row = rows[ember.y]
	if not row then
		return
	end
	rows[ember.y] = row:sub(1, ember.x - 1) .. ember.ch .. row:sub(ember.x + 1)
end

local function heat_to_pixels(sim, envelope, show_ember)
	local rows = {}
	for y = 1, fire_height() do
		local row = canonical_row(y)
		local canonical_y = y - SIM_TOP_PAD
		local out = {}
		for x = 1, #row do
			local v = sim.heat[y][x] or 0
			local canonical = row:sub(x, x)
			if canonical_y >= 5 and canonical ~= "." then
				v = math.max(v, heat_for(canonical) * (0.9 + 0.08 * envelope))
			end
			if canonical_y >= 5 and canonical == "." then
				local left = heat_for(row:sub(math.max(1, x - 1), math.max(1, x - 1)))
				local right = heat_for(row:sub(math.min(#row, x + 1), math.min(#row, x + 1)))
				if left == 0 and right == 0 then
					v = math.min(v, 0.16)
				end
			end
			out[x] = char_for_heat(v)
		end
		rows[y] = table.concat(out)
	end
	if show_ember then
		overlay_ember(rows, sim.tick)
	end
	return rows
end

local function step_sim(sim, envelope)
	sim.tick = sim.tick + 1
	local width, height = fire_width(), fire_height()
	local grow = 0.55 + 0.45 * envelope
	local wind = math.sin(sim.tick * 0.62) * 0.55 + (math.random() - 0.5) * 0.42
	local prev = sim.heat
	local next_heat = {}

	for y = 1, height do
		next_heat[y] = {}
		for x = 1, width do
			local drift = wind > 0 and -1 or 1
			local below = prev[math.min(height, y + 1)][x] or 0
			local below_drift = prev[math.min(height, y + 1)][math.max(1, math.min(width, x + drift))] or 0
			local two_below = prev[math.min(height, y + 2)][x] or 0
			local base = heat_for(canonical_row(y):sub(x, x)) * (0.5 + 0.35 * grow)
			local noise = (math.random() - 0.42) * 0.09 * grow
			next_heat[y][x] = math.max(
				0,
				math.min(1, base + below * 0.48 + below_drift * 0.24 + two_below * 0.14 + noise - y * 0.012)
			)
		end
	end

	add_source(next_heat, 8, SIM_TOP_PAD + 6, 0.44 * grow, 4, 2)
	add_source(next_heat, 8, SIM_TOP_PAD + 5, 0.27 * grow, 3, 3)
	add_source(next_heat, 15, SIM_TOP_PAD + 6, 0.28 * grow, 3, 1)
	add_source(next_heat, 15, SIM_TOP_PAD + 5, 0.2 * grow, 2, 2)
	if sim.tick >= 5 and sim.tick <= 13 then
		add_source(next_heat, 7 + math.floor((sim.tick - 5) / 3), SIM_TOP_PAD + 2, 0.2 * envelope, 2, 1)
	end

	sim.heat = next_heat
	return heat_to_pixels(sim, envelope, true)
end

local function step_cooldown(sim)
	sim.cooldown = sim.cooldown + 1
	if sim.cooldown >= SIM_COOLDOWN_TICKS then
		sim.heat = sim.canonical
		return heat_to_pixels(sim, 0, false), true
	end
	local width, height = fire_width(), fire_height()
	local blend = 0.28 + 0.16 * (sim.cooldown / SIM_COOLDOWN_TICKS)
	local next_heat = {}
	for y = 1, height do
		next_heat[y] = {}
		for x = 1, width do
			local current = sim.heat[y][x] or 0
			local target = sim.canonical[y][x] or 0
			next_heat[y][x] = current * (1 - blend) + target * blend
		end
	end
	sim.heat = next_heat
	return heat_to_pixels(sim, 0, false), false
end

local function cancel_animation()
	if state.timer then
		state.timer:remove()
	end
	state.timer = nil
	state.sim = nil
	state.fire_pixels = nil
end

local function teardown()
	cancel_animation()
	state.held = false
	if state.overlay then
		state.overlay:close()
	end
	if state.paint then
		state.paint:remove()
	end
	state.overlay = nil
	state.paint = nil
	state.version_buf = nil
	state.version_win = nil
end

local function paint_rows_for_frame()
	local fire = state.fire_pixels or banner.FIRE_PIXELS
	return banner.logo_mark_pixels(fire)
end

local function paint_logo(slice, _ctx)
	local w = banner.logo_mark_size()
	local col0 = math.max(0, math.floor((slice:width() - w) / 2))
	local row0 = state.fire_pixels and 0 or FIRE_HEADROOM
	banner.paint_pixels(slice, row0, col0, paint_rows_for_frame())
end

-- One tick of the animation loop. While `held`, advances the sim with
-- envelope ramping toward sustain. After release, blends heat toward
-- canonical until the fire snaps back, then drops the sim.
local function tick_animation()
	state.timer = nil
	if not state.overlay or not state.sim then
		return
	end
	if state.held then
		local envelope = math.min(1, state.sim.tick / SIM_RAMP_TICKS)
		state.fire_pixels = step_sim(state.sim, envelope)
	else
		local pixels, done = step_cooldown(state.sim)
		state.fire_pixels = done and nil or pixels
		if done then
			state.sim = nil
			return
		end
	end
	state.timer = smelt.timer.set(SIM_DELAY_MS, tick_animation)
end

local function on_press()
	if state.timer then
		state.timer:remove()
		state.timer = nil
	end
	state.held = true
	state.sim = seeded_sim()
	state.fire_pixels = step_sim(state.sim, math.min(1, 1 / SIM_RAMP_TICKS))
	state.timer = smelt.timer.set(SIM_DELAY_MS, tick_animation)
end

local function on_release()
	state.held = false
end

local function ensure_version_window(text)
	local buf = smelt.buf.new({ name = "smelt.banner.version.buf" })
	buf:lines({ text })
	local ns = smelt.ns("smelt.banner.version")
	buf:clear_ns(ns)
	buf:mark(ns, 1, 0, { end_col = #text, dim = true })
	local win = smelt.win.new(buf, {
		name = "smelt.banner.version.win",
		focusable = false,
		selectable = true,
	})
	state.version_buf = buf
	state.version_win = win
	return win
end

local function open_splash()
	if state.overlay then
		return
	end
	state.fire_pixels = nil
	state.paint = smelt.paint.register(paint_logo, { name = "smelt.banner.splash.paint" })
	state.paint:on("press", on_press)
	state.paint:on("release", on_release)
	local logo_w, logo_h = banner.logo_mark_size()
	local version_text = "v" .. (smelt.version or "")
	local w = math.max(logo_w, #version_text)
	-- Reserve FIRE_HEADROOM cells above the wordmark inside the paint
	-- leaf so the fire animation grows upward without painting outside the
	-- overlay rect (which would bleed under higher-z modals like /help).
	local paint_h = logo_h + FIRE_HEADROOM
	local version_win = ensure_version_window(version_text)
	-- Paint slot on top, version buffer below. `measure` pins each slot's
	-- natural width to `w` so the overlay centers exactly.
	local sized = smelt.overlay.layout.vbox({
		{
			smelt.overlay.layout.leaf(state.paint, {
				measure = { w, paint_h },
			}),
			height = paint_h,
		},
		{
			smelt.overlay.layout.leaf(version_win, { measure = { w, 1 } }),
			height = 1,
		},
	})
	state.overlay = smelt.overlay.new({
		name = "smelt.banner.splash",
		anchor = "win",
		target = smelt.win.transcript(),
		attach = "center",
		-- The transcript's bottom gap row pulls integer-center math half a
		-- row above true center on odd heights; nudge down by 1.
		row_offset = 1,
		-- Sits behind dialogs and plugin overlays (default z = 50).
		z = 0,
		modal = false,
		blocks_agent = false,
		border = "none",
		layout = sized,
	})
	-- Center the version text inside the bottom slot via leading padding.
	local pad = math.floor((w - #version_text) / 2)
	if pad > 0 then
		state.version_buf:lines({ string.rep(" ", pad) .. version_text })
		local ns = smelt.ns("smelt.banner.version")
		state.version_buf:clear_ns(ns)
		state.version_buf:mark(ns, 1, pad, { end_col = pad + #version_text, dim = true })
	end
end

local function refresh()
	local msgs = smelt.session.messages({}) or {}
	if #msgs == 0 then
		open_splash()
	else
		teardown()
	end
end

-- session_started covers /reset, /fork, /resume; turn_start covers the
-- first dispatch; history covers rewind / compaction / load. on_ready
-- ensures the host pointer is live before the first paint.
smelt.cell("session_started"):subscribe(refresh)
smelt.cell("turn_start"):subscribe(teardown)
smelt.cell("history"):subscribe(refresh)
smelt.lifecycle.on_ready(refresh)

smelt.lifecycle.on_shutdown(function(ctx)
	if not ctx.has_messages then
		return
	end
	local rows = banner.LOGO_MARK_PIXELS
	local version_text = "v" .. (smelt.version or "")
	local pad = math.max(0, math.floor((#rows[1] - #version_text) / 2))
	print(banner.ansi_render(rows, banner.PALETTE))
	print(string.rep(" ", pad) .. "\27[2m" .. version_text .. "\27[0m")
	print("")
	io.write(string.format("\27[2mresume with:\nsmelt --resume %s\27[0m\n\n", ctx.session_id))
end)
