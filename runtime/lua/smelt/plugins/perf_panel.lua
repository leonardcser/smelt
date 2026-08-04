-- F12 perf panel. Top-right overlay showing live duration percentiles.
-- Non-modal, focusable, read-only; F12 toggles it, q, Esc, or Ctrl-C closes.
--
-- Hot-reload contract: every resource opens with `opts.name`, so on
-- `/reload` the runtime hands back the existing overlay/win/buf and
-- re-applies the mutable subset of opts. The `smelt.state` table
-- preserves open state across reload so we re-arm the timer.

local M = {}

local state = smelt.state.get("perf_panel")
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
		return "SmeltSuccess"
	end
	if us < 5000 then
		return "SmeltAccent"
	end
	if us < 16000 then
		return "WarningMsg"
	end
	return "ErrorMsg"
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
	return smelt.dialog.title({
		{ text = " performance ", bold = true },
		{ text = "(F12 to close) ", fg = "grey" },
	})
end

local function current_layout(win)
	local rect = win:rect()
	if not rect then
		return MIN_LABEL_W, PANEL_H - 1
	end
	local inner_w = math.max(rect.width - 2, 0)
	local lw = inner_w - STATS_W
	if lw < MIN_LABEL_W then
		lw = MIN_LABEL_W
	end
	return lw, math.max((rect.height or PANEL_H) - 1, 1)
end

local function ordered_rows(rows)
	state.row_order = state.row_order or {}
	state.row_seen = state.row_seen or {}
	local by_label = {}
	for _, r in ipairs(rows or {}) do
		by_label[r.label] = r
		if not state.row_seen[r.label] then
			state.row_seen[r.label] = true
			state.row_order[#state.row_order + 1] = r.label
		end
	end
	local out = {}
	for _, label in ipairs(state.row_order) do
		if by_label[label] then
			out[#out + 1] = by_label[label]
		end
	end
	return out
end

local function compose_lines(snap, label_w, max_rows)
	local header = header_for(label_w)
	local lines = { header }
	local color_spans = {
		{ row = 1, col = 0, end_col = #header, role = "Comment" },
	}
	local rows = ordered_rows(snap.durations or {})
	local n = math.min(#rows, math.max(max_rows - 1, 0))
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
	if #rows == 0 then
		lines[#lines + 1] = "  (no samples yet)"
	end
	return lines, color_spans
end

local function paint_body()
	if not state.open then
		return
	end
	local buf, win = state.buf, state.win
	if not buf or not win then
		return
	end
	local label_w, max_rows = current_layout(win)
	local ok, snap = pcall(smelt.metrics.perf.snapshot_top, math.max(max_rows - 1, 1))
	if not ok then
		return
	end
	local lines, spans = compose_lines(snap, label_w, max_rows)
	buf:lines(lines):clear_ns(NS_HL)
	for _, sp in ipairs(spans) do
		buf:mark(NS_HL, sp.row, sp.col, { end_col = sp.end_col, fg = sp.role })
	end
end

local function paint()
	if smelt.perf and smelt.perf.time then
		return smelt.perf.time("perf_panel:paint", paint_body)
	end
	return paint_body()
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
		surface = "readonly_text",
		hide_cursor = true,
		vim_enabled = smelt.settings.vim and true or false,
	})
	state.win:key("esc", close)
	state.win:key("q", close)
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
	state.win:focus()
	-- Cancel any prior timer (hot-reload survival) before re-arming.
	if state.timer then
		state.timer:remove()
	end
	state.timer = smelt.timer.every(250, paint)
	paint()
end

local function open()
	state.open = true
	state.row_order = {}
	state.row_seen = {}
	-- If perf is already on (e.g. `--bench`), leave its samples and enabled
	-- flag alone so the end-of-run summary still has data to print.
	state.owns_perf = not smelt.metrics.perf.enabled()
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
