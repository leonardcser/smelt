-- Which-key style popup for pending global Lua keymaps. Opt-in with
-- `require("smelt.plugins.which_key")`.

local M = {}

local state = smelt.state.get("which_key")
local NS_HL = smelt.ns("smelt.which_key")

local DEFAULTS = {
	width = 48,
	max_rows = 12,
	corner = "ne",
	row = 0,
	col = 0,
}

local function opts()
	state.opts = state.opts or {}
	for k, v in pairs(DEFAULTS) do
		if state.opts[k] == nil then
			state.opts[k] = v
		end
	end
	return state.opts
end

local function fit(s, width)
	if smelt.text and smelt.text.fit then
		return smelt.text.fit(s, width)
	end
	if #s > width then
		return s:sub(1, math.max(width - 1, 0)) .. "…"
	end
	return s .. string.rep(" ", math.max(width - #s, 0))
end

local function collect_rows(pending)
	return smelt.keymap.prefixes(pending)
end

local function title(pending)
	return smelt.dialog.title({
		{ text = " which-key ", bold = true },
		{ text = pending .. " ", fg = "grey" },
	})
end

local function close()
	state.open = false
	if state.overlay then
		state.overlay:close()
		state.overlay = nil
	end
	state.win = nil
end

local function paint(rows)
	if not state.buf then
		return
	end
	local cfg = opts()
	local lines = {}
	local spans = {}
	local suffix_w = math.min(12, math.max(cfg.width - 8, 1))
	local desc_w = math.max(cfg.width - suffix_w - 5, 1)
	local shown = math.min(#rows, cfg.max_rows)

	for i = 1, shown do
		local row = rows[i]
		local suffix = fit(row.suffix or "", suffix_w)
		local desc = fit(row.desc or "(keymap)", desc_w)
		lines[#lines + 1] = "  " .. suffix .. "  " .. desc
		spans[#spans + 1] = { row = i, col = 3, end_col = 3 + #suffix, fg = "SmeltAccent", bold = true }
		spans[#spans + 1] = { row = i, col = 3 + #suffix + 2, end_col = cfg.width, fg = "Comment" }
	end

	if #rows > shown then
		lines[#lines + 1] = "  … " .. tostring(#rows - shown) .. " more"
	elseif shown == 0 then
		lines[#lines + 1] = "  no longer mappings"
	end

	state.buf:lines(lines):clear_ns(NS_HL)
	for _, sp in ipairs(spans) do
		state.buf:mark(NS_HL, sp.row, sp.col, { end_col = sp.end_col, fg = sp.fg, bold = sp.bold })
	end
end

local function attach(pending, rows)
	if state.overlay then
		state.overlay:close()
		state.overlay = nil
	end
	local cfg = opts()
	state.buf = smelt.buf.new({ name = "which_key.buf", readonly = true })
	state.win = smelt.win.new(state.buf, {
		name = "which_key.win",
		surface = "inert",
		vim_enabled = false,
	})
	local height = math.max(1, math.min(#rows, cfg.max_rows) + (#rows > cfg.max_rows and 1 or 0))
	state.overlay = smelt.overlay.new({
		name = "which_key",
		title = title(pending),
		anchor = "screen_at",
		corner = cfg.corner,
		row = cfg.row,
		col = cfg.col,
		border = { all = "Comment" },
		modal = false,
		blocks_agent = false,
		draggable = true,
		resizable = true,
		layout = smelt.ui.layout.leaf(state.win, { measure = { cfg.width, height } }),
	})
	paint(rows)
end

local function update(pending)
	pending = pending or ""
	if pending == "" then
		close()
		return
	end

	local rows = collect_rows(pending)
	if #rows == 0 then
		close()
		return
	end

	state.open = true
	attach(pending, rows)
end

function M.setup(user_opts)
	local cfg = opts()
	for k, v in pairs(user_opts or {}) do
		cfg[k] = v
	end
	if state.subscription then
		state.subscription:remove()
	end
	state.subscription = smelt.signal.subscribe("keymap_pending", update)
	update(smelt.signal.get("keymap_pending"))
	return M
end

M.setup(state.opts)

return M
