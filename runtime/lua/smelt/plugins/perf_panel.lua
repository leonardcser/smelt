-- F12 perf panel. Top-right overlay showing live duration percentiles.
-- Non-modal, focusable, read-only; F12 toggles it, Esc or Ctrl-C closes.
--
-- Hot-reload contract: every resource opens with `opts.name`, so on
-- `/reload` the runtime hands back the existing overlay/win/buf and
-- re-applies the mutable subset of opts. The `smelt.state` table
-- preserves open state across reload so we re-arm the timer.

local M = {}

local state = smelt.state("perf_panel")
local NS_HL = smelt.ns("smelt.perf_panel")

local PANEL_W = 44
local PANEL_H = 14
-- STATS_W = right-hand "last  p99   n" cluster width; label column = inner_w - STATS_W.
local STATS_W = 19
local MIN_LABEL_W = 6

local function severity_role(us)
	if us < 100 then
		return "Comment"
	end
	if us < 1000 then
		return "SmeltReasonLow"
	end
	if us < 5000 then
		return "SmeltReasonMed"
	end
	if us < 16000 then
		return "SmeltReasonHigh"
	end
	return "SmeltReasonMax"
end

local function fmt_us(us)
	if us < 1000 then
		return string.format("%4dµs", us)
	end
	local ms = us / 1000.0
	if ms < 10 then
		return string.format("%4.2fms", ms)
	end
	if ms < 100 then
		return string.format("%4.1fms", ms)
	end
	return string.format("%4dms", math.floor(ms + 0.5))
end

local function pad_label(label, label_w)
	local len = #label
	if len > label_w then
		return label:sub(1, label_w - 1) .. "…"
	end
	return label .. string.rep(" ", label_w - len)
end

local function header_for(label_w)
	return pad_label("label", label_w)
		.. " "
		.. string.format("%6s", "last")
		.. "  "
		.. string.format("%6s", "p99")
		.. " "
		.. string.format("%3s", "n")
end

local function panel_title()
	return {
		{ text = " performance ", bold = true },
		{ text = "(F12 to close) ", fg = "grey", dim = true },
	}
end

local function current_label_width(win)
	local rect = win:rect()
	if not rect then
		return MIN_LABEL_W
	end
	local inner_w = math.max(rect.width - 2, 0)
	local lw = inner_w - STATS_W
	if lw < MIN_LABEL_W then
		return MIN_LABEL_W
	end
	return lw
end

local function compose_lines(snap, label_w)
	local lines = { header_for(label_w) }
	local color_spans = {}
	local rows = snap.durations or {}
	local max_rows = PANEL_H - 3
	local n = math.min(#rows, max_rows)
	for i = 1, n do
		local r = rows[i]
		local last_s = fmt_us(r.last_us)
		local p99_s = fmt_us(r.p99_us)
		local cnt_s = string.format("%3d", math.min(r.count, 999))
		local label_s = pad_label(r.label, label_w)
		local line = label_s .. " " .. last_s .. "  " .. p99_s .. " " .. cnt_s
		lines[#lines + 1] = line
		-- Byte offsets into `line`. `pad_label` pads with ASCII spaces, so
		-- #label_s == cells of the label column; `last_s`/`p99_s` may contain
		-- the µ glyph (2 bytes / 1 cell), so we measure via `#` not width.
		local last_col = #label_s + 1
		local p99_col = last_col + #last_s + 2
		table.insert(
			color_spans,
			{ row = i + 1, col = last_col, end_col = last_col + #last_s, role = severity_role(r.last_us) }
		)
		table.insert(
			color_spans,
			{ row = i + 1, col = p99_col, end_col = p99_col + #p99_s, role = severity_role(r.p99_us) }
		)
	end
	if n == 0 then
		lines[#lines + 1] = "  (no samples yet)"
	end
	return lines, color_spans
end

local function paint()
	if not state.open then
		return
	end
	local buf, win = state.buf, state.win
	if not buf or not win then
		return
	end
	local ok, snap = pcall(smelt.metrics.perf.snapshot)
	if not ok then
		return
	end
	local label_w = current_label_width(win)
	local lines, spans = compose_lines(snap, label_w)
	buf:lines(lines):clear_ns(NS_HL)
	for _, sp in ipairs(spans) do
		buf:mark(NS_HL, sp.row, sp.col, { end_col = sp.end_col, fg = sp.role })
	end
end

local function close()
	state.open = false
	if state.timer then
		state.timer:remove()
		state.timer = nil
	end
	if state.overlay then
		state.overlay:close()
		state.overlay = nil
	end
	state.win = nil
	-- Named buf survives for next open by design.
	if state.owns_perf then
		smelt.metrics.perf.set_enabled(false)
		smelt.metrics.perf.clear()
		state.owns_perf = nil
	end
end

local function attach()
	state.buf = smelt.buf.new({ name = "perf_panel.buf", readonly = true })
	state.win = smelt.win.new(state.buf, {
		name = "perf_panel.win",
		focusable = true,
		selectable = true,
		vim_enabled = smelt.settings.vim and true or false,
	})
	state.win:key("esc", close)
	state.win:key("c-c", close)
	state.overlay = smelt.overlay.new({
		name = "perf_panel",
		title = panel_title(),
		anchor = "screen_at",
		corner = "ne",
		row = 0,
		col = 0,
		border = { all = "Comment" },
		modal = false,
		blocks_agent = false,
		draggable = true,
		resizable = true,
		layout = smelt.ui.layout.leaf(state.win, { measure = { PANEL_W, PANEL_H } }),
	})
	-- Cancel any prior timer (hot-reload survival) before re-arming.
	if state.timer then
		state.timer:remove()
	end
	state.timer = smelt.timer.every(250, paint)
	paint()
end

local function open()
	state.open = true
	-- If perf is already on (e.g. `--bench`), leave its samples and enabled
	-- flag alone so the end-of-run summary still has data to print.
	state.owns_perf = not smelt.metrics.perf.snapshot().enabled
	if state.owns_perf then
		smelt.metrics.perf.clear()
		smelt.metrics.perf.set_enabled(true)
	end
	attach()
end

local function toggle()
	if state.open then
		close()
	else
		open()
	end
end

smelt.keymap.set("", "<F12>", toggle)

-- Re-attach after /reload: named resources survive, paint timer is anonymous.
if state.open then
	attach()
end

return M
