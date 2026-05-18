-- /banner — show the smelt logo in a draggable overlay.
--
-- Demo of `smelt.overlay` + `smelt.paint` reusing canonical pixel data
-- from `smelt.banner`. Esc / Ctrl-C closes.
--
-- Not autoloaded. Add `require("smelt.examples.banner_picker")` to
-- `init.lua` to wire the `/banner` command.
--
-- Hot-reload survival: `is_open` lives in
-- `smelt.state("smelt.banner_picker")`; the overlay, window, buffer,
-- and paint slot carry `name = "..."` so their backing structures
-- survive `/reload` and pick up the freshly-loaded closures. On bring-up
-- the tail re-opens the picker if it was open at reload time.

local banner = require("smelt.banner")

local M = {}

local function persist()
	local s = smelt.state("smelt.banner_picker")
	if s.is_open == nil then s.is_open = false end
	return s
end

local STATE = nil

local function logo_info()
	local version = "v" .. (smelt.version or "")
	local rows = banner.LOGO_MARK_PIXELS
	local w, h = banner.logo_mark_size()
	return w,
		h + 1,
		function(slice, r, c)
			banner.paint_pixels(slice, r, c, rows)
			local pad = math.floor((w - #version) / 2)
			slice:put_str(r + h, c + pad, version, { dim = true })
		end
end

local function paint(slice, _ctx)
	if not STATE then return end
	local w, h, paint_fn = logo_info()
	local r0 = math.max(0, math.floor((slice:height() - h) / 2))
	local c0 = math.max(0, math.floor((slice:width() - w) / 2))
	paint_fn(slice, r0, c0)
end

local function close()
	if not STATE then return end
	if STATE.paint then STATE.paint:remove() end
	if STATE.win then STATE.win:close() end
	if STATE.overlay then STATE.overlay:close() end
	STATE = nil
	persist().is_open = false
end

local function open()
	if STATE then return end
	STATE = {}

	local w, h = logo_info()

	-- Zero-height invisible window anchors focus inside the overlay so
	-- overlay-scoped keymaps route correctly.
	STATE.buf = smelt.buf.new({ name = "smelt.banner_picker.focus.buf" })
	STATE.win = smelt.win.new(STATE.buf, {
		name = "smelt.banner_picker.focus.win",
		focusable = true,
	})

	STATE.paint = smelt.paint.register(paint, { name = "smelt.banner_picker.paint" })

	STATE.overlay = smelt.overlay.new({
		name = "smelt.banner_picker",
		anchor = "center",
		border = "none",
		layout = smelt.overlay.layout.vbox({
			{
				smelt.overlay.layout.leaf(STATE.paint, { measure = { w, h } }),
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
			{ key = "<Esc>", on_press = close },
			{ key = "<C-c>", on_press = close },
		},
	})
	STATE.win:focus()

	persist().is_open = true
end

local function toggle()
	if STATE then close() else open() end
end

smelt.cmd.register("banner", toggle, { desc = "show the smelt logo (demo)" })

if persist().is_open then
	open()
end

return M
