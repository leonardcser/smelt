-- F3 debug panel. Top-left overlay showing resolved model config,
-- context tokens, cache info, and other small debug details.
-- Non-modal, focusable, read-only; F3 toggles it, Esc or Ctrl-C closes.
--
-- Hot-reload contract: every resource opens with `opts.name`, so on
-- `/reload` the runtime hands back the existing overlay/win/buf and
-- re-applies the mutable subset of opts. The `smelt.state` table
-- preserves open state across reload so we re-arm the timer.

local M = {}

local state = smelt.state("debug_panel")
local NS_HL = smelt.ns("smelt.debug_panel")

local PANEL_W = 52
local PANEL_H = 20

local function panel_title()
	return {
		{ text = " debug ", bold = true },
		{ text = "(F3 to close) ", fg = "grey", dim = true },
	}
end

local function fmt_opt(val, fmt_fn)
	if val == nil then
		return "nil"
	end
	if fmt_fn then
		return fmt_fn(val)
	end
	return tostring(val)
end

local function fmt_bool(v)
	return v and "true" or "false"
end

local function fmt_num(v)
	if v == math.floor(v) then
		return string.format("%d", v)
	end
	return string.format("%.4g", v)
end

local function fmt_pct(num, denom)
	if not num or not denom or denom == 0 then
		return "nil"
	end
	return string.format("%.1f%%", num / denom * 100)
end

local function pad_kv(key, val, width)
	local key_str = tostring(key)
	local val_str = tostring(val)
	local sep = " "
	local avail = math.max(width - #key_str - #sep, 0)
	if #val_str > avail then
		val_str = val_str:sub(1, avail - 1) .. "…"
	end
	return key_str .. sep .. string.rep(".", avail - #val_str) .. val_str
end

local function compose_lines()
	local lines = {}
	local spans = {}
	local width = PANEL_W - 2

	local model = smelt.model() or ""
	local provider = smelt.config.provider_type()
	local api_base = smelt.config.api_base()
	local mode = smelt.mode() or ""
	local reasoning = smelt.reasoning() or ""

	local ctx = smelt.session.context_tokens()
	local window = smelt.session.context_window()
	local cost = smelt.session.cost()
	local tokens = smelt.session.tokens()
	local turns = smelt.session.turns()
	local messages = smelt.session.messages()
	local work_state = smelt.cell("work_state"):get() or "idle"

	local pricing = smelt.model.pricing()
	local mcfg = smelt.config.model_config()

	-- Header: model + provider
	lines[#lines + 1] = pad_kv("model", model, width)
	lines[#lines + 1] = pad_kv("provider", provider, width)
	lines[#lines + 1] = pad_kv("api_base", api_base, width)
	lines[#lines + 1] = pad_kv("mode", mode .. (reasoning ~= "off" and " / " .. reasoning or ""), width)

	-- Context
	local ctx_str
	if ctx and window and window > 0 then
		ctx_str = string.format("%s / %s (%s)", smelt.text.format_tokens(ctx), smelt.text.format_tokens(window), fmt_pct(ctx, window))
	elseif ctx then
		ctx_str = smelt.text.format_tokens(ctx)
	else
		ctx_str = "nil"
	end
	lines[#lines + 1] = pad_kv("context", ctx_str, width)
	local max_tok = smelt.model.max_tokens()
	lines[#lines + 1] = pad_kv("max_tokens", max_tok and fmt_num(max_tok) or "default", width)

	-- Cost & pricing
	lines[#lines + 1] = pad_kv("cost", cost and cost > 0 and smelt.text.format_cost(cost) or "0", width)
	lines[#lines + 1] = pad_kv("pricing", pricing.source, width)

	-- Tokens
	local tok_parts = {}
	if tokens.input and tokens.input > 0 then
		tok_parts[#tok_parts + 1] = "in=" .. smelt.text.format_tokens(tokens.input)
	end
	if tokens.output and tokens.output > 0 then
		tok_parts[#tok_parts + 1] = "out=" .. smelt.text.format_tokens(tokens.output)
	end
	if tokens.cache_read and tokens.cache_read > 0 then
		tok_parts[#tok_parts + 1] = "cr=" .. smelt.text.format_tokens(tokens.cache_read)
	end
	if tokens.cache_write and tokens.cache_write > 0 then
		tok_parts[#tok_parts + 1] = "cw=" .. smelt.text.format_tokens(tokens.cache_write)
	end
	if tokens.reasoning and tokens.reasoning > 0 then
		tok_parts[#tok_parts + 1] = "rsn=" .. smelt.text.format_tokens(tokens.reasoning)
	end
	local tok_str = #tok_parts > 0 and table.concat(tok_parts, " ") or "nil"
	lines[#lines + 1] = pad_kv("tokens", tok_str, width)

	if tokens.cache_hit_ratio then
		lines[#lines + 1] = pad_kv("cache_hit", string.format("%.1f%%", tokens.cache_hit_ratio * 100), width)
	else
		lines[#lines + 1] = pad_kv("cache_hit", "nil", width)
	end

	-- Model config
	local cfg_parts = {}
	if mcfg.temperature ~= nil then
		cfg_parts[#cfg_parts + 1] = "temp=" .. fmt_num(mcfg.temperature)
	end
	if mcfg.top_p ~= nil then
		cfg_parts[#cfg_parts + 1] = "top_p=" .. fmt_num(mcfg.top_p)
	end
	if mcfg.top_k ~= nil then
		cfg_parts[#cfg_parts + 1] = "top_k=" .. fmt_num(mcfg.top_k)
	end
	if mcfg.tool_calling ~= nil then
		cfg_parts[#cfg_parts + 1] = "tools=" .. fmt_bool(mcfg.tool_calling)
	end
	if mcfg.min_p ~= nil then
		cfg_parts[#cfg_parts + 1] = "min_p=" .. fmt_num(mcfg.min_p)
	end
	if mcfg.repeat_penalty ~= nil then
		cfg_parts[#cfg_parts + 1] = "rp=" .. fmt_num(mcfg.repeat_penalty)
	end
	lines[#lines + 1] = pad_kv("sampling", #cfg_parts > 0 and table.concat(cfg_parts, " ") or "defaults", width)

	if mcfg.thinking_budgets then
		local b = mcfg.thinking_budgets
		lines[#lines + 1] = pad_kv(
			"thinking",
			string.format("low=%s med=%s high=%s max=%s", smelt.text.format_tokens(b.low), smelt.text.format_tokens(b.medium), smelt.text.format_tokens(b.high), smelt.text.format_tokens(b.max)),
			width
		)
	end

	if mcfg.input_cost or mcfg.output_cost then
		local cost_parts = {}
		if mcfg.input_cost then
			cost_parts[#cost_parts + 1] = "in=" .. fmt_num(mcfg.input_cost)
		end
		if mcfg.output_cost then
			cost_parts[#cost_parts + 1] = "out=" .. fmt_num(mcfg.output_cost)
		end
		if mcfg.cache_read_cost then
			cost_parts[#cost_parts + 1] = "cr=" .. fmt_num(mcfg.cache_read_cost)
		end
		if mcfg.cache_write_cost then
			cost_parts[#cost_parts + 1] = "cw=" .. fmt_num(mcfg.cache_write_cost)
		end
		lines[#lines + 1] = pad_kv("cost_override", table.concat(cost_parts, " "), width)
	end

	-- Session meta
	lines[#lines + 1] = pad_kv("turns", tostring(#turns), width)
	lines[#lines + 1] = pad_kv("messages", tostring(#messages), width)
	lines[#lines + 1] = pad_kv("work_state", work_state, width)

	-- Highlight keys in Comment
	for i, line in ipairs(lines) do
		local dot_idx = line:find("%.", 1, true)
		if dot_idx then
			-- key runs from start up to (but not including) the first dot
			table.insert(spans, { row = i, col = 1, end_col = dot_idx, fg = "Comment" })
		end
	end

	return lines, spans
end

local function paint()
	if not state.open then
		return
	end
	local buf, win = state.buf, state.win
	if not buf or not win then
		return
	end
	local ok, lines, spans = pcall(compose_lines)
	if not ok then
		return
	end
	buf:lines(lines):clear_ns(NS_HL)
	for _, sp in ipairs(spans) do
		buf:mark(NS_HL, sp.row, sp.col, { end_col = sp.end_col, fg = sp.role or "Comment" })
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
end

local function attach()
	state.buf = smelt.buf.new({ name = "debug_panel.buf", readonly = true })
	state.win = smelt.win.new(state.buf, { name = "debug_panel.win", focusable = true, selectable = true })
	state.win:key("esc", close)
	state.win:key("c-c", close)
	state.overlay = smelt.overlay.new({
		name = "debug_panel",
		title = panel_title(),
		anchor = "screen_at",
		corner = "nw",
		row = 0,
		col = 0,
		border = { all = "Comment" },
		modal = false,
		blocks_agent = false,
		draggable = true,
		resizable = true,
		layout = smelt.ui.layout.leaf(state.win, { measure = { PANEL_W, PANEL_H } }),
	})
	if state.timer then
		state.timer:remove()
	end
	state.timer = smelt.timer.every(500, paint)
	paint()
end

local function open()
	state.open = true
	attach()
end

local function toggle()
	if state.open then
		close()
	else
		open()
	end
end

smelt.keymap.set("", "<F3>", toggle)

if state.open then
	attach()
end

return M
