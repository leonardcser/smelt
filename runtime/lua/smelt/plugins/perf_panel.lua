-- F12 perf panel. Top-right overlay showing live duration percentiles.
-- Non-modal, non-focusable; F12 toggles it.
--
-- Hot-reload contract: every resource opens with `opts.name`, so on
-- `/reload` the runtime hands back the existing overlay/window/buffer
-- and re-applies the mutable subset of opts (title, border, …). The
-- `smelt.state` table preserves the "is the panel currently open?"
-- bool across reload so we know whether to re-arm the timer.

local M = {}

local state = smelt.state("perf_panel")
local NS_HL = smelt.buf.create_namespace("smelt.perf_panel")

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
	local label_col = pad_label("label", label_w)
	local last_col = string.format("%6s", "last")
	local p99_col = string.format("%6s", "p99")
	local cnt_col = string.format("%3s", "n")
	return label_col .. " " .. last_col .. "  " .. p99_col .. " " .. cnt_col
end

local function panel_title()
	return {
		{ text = " perf ", bold = true },
		{ text = "(F12 to close) ", fg = "grey", dim = true },
	}
end

local function current_label_width(win_id)
	local rect = smelt.win.rect(win_id)
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
	local width = smelt.text.width
	for i = 1, n do
		local r = rows[i]
		local last_s = fmt_us(r.last_us)
		local p99_s = fmt_us(r.p99_us)
		local cnt_s = string.format("%3d", math.min(r.count, 999))
		local line = pad_label(r.label, label_w) .. " " .. last_s .. "  " .. p99_s .. " " .. cnt_s
		lines[#lines + 1] = line
		local last_w = width(last_s)
		local p99_w = width(p99_s)
		local last_col = label_w + 1
		table.insert(color_spans, {
			row = i + 1,
			col = last_col,
			end_col = last_col + last_w,
			role = severity_role(r.last_us),
		})
		local p99_col = last_col + last_w + 2
		table.insert(color_spans, {
			row = i + 1,
			col = p99_col,
			end_col = p99_col + p99_w,
			role = severity_role(r.p99_us),
		})
	end
	if n == 0 then
		lines[#lines + 1] = "  (no samples yet)"
	end
	return lines, color_spans
end

local function paint()
	local buf = smelt.buf.named("perf_panel.buf")
	local win = smelt.win.named("perf_panel.win")
	if not buf or not win then
		return
	end
	local ok, snap = pcall(smelt.metrics.perf.snapshot)
	if not ok then
		return
	end
	local label_w = current_label_width(win)
	local lines, spans = compose_lines(snap, label_w)
	smelt.buf.set_lines(buf, lines)
	smelt.buf.clear_namespace(buf, NS_HL)
	for _, sp in ipairs(spans) do
		smelt.buf.set_extmark(buf, NS_HL, sp.row, sp.col, {
			end_col = sp.end_col,
			fg = sp.role,
		})
	end
end

-- Show / refresh the panel UI. Idempotent: safe to call on first open,
-- on re-attach after /reload, and on title/layout edits — Rust-side
-- named-resource lookups hand back the existing buf/win/overlay.
local function attach()
	local buf = smelt.buf.create({ name = "perf_panel.buf" })
	local win = smelt.win.open(buf, { name = "perf_panel.win", focusable = false })
	smelt.ui.overlay.open({
		name = "perf_panel",
		title = panel_title(),
		anchor = "screen_at",
		corner = "ne",
		row = 0,
		col = 0,
		width = PANEL_W,
		height = PANEL_H,
		border = { all = "Comment" },
		modal = false,
		blocks_agent = false,
		draggable = true,
		resizable = true,
		layout = smelt.ui.layout.leaf(win),
	})
	-- Timer is anonymous; reload cancels it wholesale, this re-arms.
	smelt.timer.every(250, paint)
	paint()
end

-- Fresh user-triggered open: clear samples, enable metrics, then attach.
local function open()
	state.open = true
	smelt.metrics.perf.clear()
	smelt.metrics.perf.set_enabled(true)
	attach()
end

local function close()
	state.open = false
	smelt.ui.overlay.close("perf_panel")
	smelt.metrics.perf.set_enabled(false)
	smelt.metrics.perf.clear()
end

local function toggle()
	if state.open then
		close()
	else
		open()
	end
end

smelt.keymap.set("", "<F12>", toggle)

-- Re-attach after /reload: rebind the named overlay/win/buf to the new
-- module's paint closure and re-arm the timer — without clearing the
-- metric samples accumulated before the reload. `metrics.perf` enable
-- state is sticky on the Rust side, so we don't toggle it here.
if state.open then
	attach()
end

return M
