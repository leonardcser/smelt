-- F3 debug panel. Top-left overlay showing resolved model config,
-- context tokens, cache info, and other small debug details.
-- Non-modal, focusable, read-only; F3 toggles it, q, Esc, or Ctrl-C closes.
--
-- Hot-reload contract: every resource opens with `opts.name`, so on
-- `/reload` the runtime hands back the existing overlay/win/buf and
-- re-applies the mutable subset of opts. The `smelt.state` table
-- preserves open state across reload so we re-arm the timer.

local M = {}

local state = smelt.state.get("debug_panel")
local NS_HL = smelt.ns("smelt.debug_panel")

local PANEL_W = 52
local PANEL_H = 30

local function panel_title()
	return smelt.dialog.title({
		{ text = " debug ", bold = true },
		{ text = "(F3 to close) ", fg = "grey" },
	})
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

local KEY_W = 14

local function fit_value(value, width)
	return smelt.text.fit(tostring(value or "nil"), math.max(width, 0))
end

local function add_kv(lines, spans, key, val, width)
	local key_str = smelt.text.fit(tostring(key), KEY_W, { suffix = "…" })
	local val_str = fit_value(val, math.max(width - KEY_W, 0))
	local line = key_str .. val_str
	lines[#lines + 1] = line
	table.insert(spans, { row = #lines, col = 0, end_col = #key_str, fg = "Comment" })
end

local function join_array(values, sep)
	if type(values) ~= "table" then return "nil" end
	local out = {}
	for _, value in ipairs(values) do
		out[#out + 1] = tostring(value)
	end
	return #out > 0 and table.concat(out, sep or ",") or "nil"
end

local function compact_stats()
	local compact = smelt.state.get("compact")
	local total = compact.total or 0
	local auto = compact.auto or 0
	local manual = compact.manual or 0
	local recovery = compact.recovery or 0
	local parts = { tostring(total) }
	if auto > 0 then parts[#parts + 1] = "auto=" .. auto end
	if manual > 0 then parts[#parts + 1] = "manual=" .. manual end
	if recovery > 0 then parts[#parts + 1] = "recovery=" .. recovery end
	return table.concat(parts, "  ")
end

local function compose_lines(win)
	local lines = {}
	local spans = {}
	local width = PANEL_W - 2
	if win then
		width = math.max(win:content_width() or width, KEY_W + 8)
	end

	local model = smelt.model.current() or ""
	local provider = smelt.config.provider_type()
	local api_base = smelt.config.api_base()
	local mode = smelt.mode.current() or ""
	local reasoning = smelt.reasoning.current() or ""
	local session_status = smelt.session.status()
	local fast = session_status.fast or {}

	local ctx = smelt.session.context_tokens()
	local window = smelt.session.context_window()
	local cost = smelt.session.cost()
	local tokens = smelt.session.tokens()
	local turns = smelt.session.turns()
	local messages = smelt.session.messages.list()
	local session_id = smelt.session.id()
	local session_title = smelt.session.title.get()
	local cwd = smelt.session.cwd()
	local worktree_managed = smelt.signal.get("cwd_managed_worktree")
	local worktree_path = smelt.signal.get("cwd_worktree_path")
	local worktree_name = smelt.signal.get("cwd_worktree")
	local work_state = smelt.signal.get("work_state") or "idle"
	local compact = smelt.state.get("compact")

	local pricing = smelt.model.pricing() or { source = "none" }
	local mcfg = smelt.config.model_config() or {}
	local caps = smelt.model.capabilities() or {}

	add_kv(lines, spans, "model", model, width)
	add_kv(lines, spans, "provider", provider, width)
	add_kv(lines, spans, "api_base", api_base, width)
	add_kv(lines, spans, "mode", mode .. (reasoning ~= "off" and " / " .. reasoning or ""), width)
	add_kv(lines, spans, "fast_mode", string.format("active=%s supported=%s", fmt_bool(fast.active), fmt_bool(fast.supported)), width)
	add_kv(lines, spans, "session_id", session_id, width)
	if session_title then
		add_kv(lines, spans, "title", session_title, width)
	end
	add_kv(lines, spans, "cwd", cwd, width)
	if worktree_managed then
		local worktree_display = worktree_path and worktree_path ~= "" and worktree_path or worktree_name
		add_kv(lines, spans, "worktree", worktree_display, width)
	end

	local ctx_str
	if window and window > 0 then
		local used = ctx or 0
		ctx_str = string.format("%s / %s (%s)", smelt.text.format_tokens(used), smelt.text.format_tokens(window), fmt_pct(used, window))
	elseif ctx then
		ctx_str = smelt.text.format_tokens(ctx)
	else
		ctx_str = "nil"
	end
	add_kv(lines, spans, "context", ctx_str, width)
	local max_tok = caps.max_tokens or smelt.model.max_tokens()
	add_kv(lines, spans, "max_tokens", max_tok and fmt_num(max_tok) or "default", width)
	add_kv(lines, spans, "modalities", join_array(caps.input_modalities, ","), width)
	local cap_parts = {}
	cap_parts[#cap_parts + 1] = "img=" .. fmt_bool(caps.supports_image)
	cap_parts[#cap_parts + 1] = "pdf=" .. fmt_bool(caps.supports_pdf)
	cap_parts[#cap_parts + 1] = "vid=" .. fmt_bool(caps.supports_video)
	if caps.supports_reasoning ~= nil then
		cap_parts[#cap_parts + 1] = "rsn=" .. fmt_bool(caps.supports_reasoning)
	end
	cap_parts[#cap_parts + 1] = "tools=" .. fmt_bool(caps.tool_calling)
	add_kv(lines, spans, "capabilities", table.concat(cap_parts, " "), width)
	local transport = caps.transport or {}
	add_kv(lines, spans, "transport", string.format(
		"image_tools=%s pdf_tools=%s",
		fmt_bool(transport.image_tool_results),
		fmt_bool(transport.pdf_tool_results)
	), width)
	local sources = caps.sources or {}
	add_kv(lines, spans, "cap_sources", string.format("mods=%s ctx=%s max=%s", sources.input_modalities or "?", sources.context_window or "?", sources.max_tokens or "?"), width)
	add_kv(lines, spans, "auto_compact", fmt_bool(smelt.settings.auto_compact), width)
	add_kv(lines, spans, "threshold", string.format("%.0f%%", (smelt.settings.compact_threshold or 0.8) * 100), width)
	add_kv(lines, spans, "compactions", compact_stats(), width)
	add_kv(lines, spans, "compact_fail", string.format("%s  consecutive=%s", compact.failures or 0, compact.consecutive_failures or 0), width)
	if compact.last_phase then
		add_kv(lines, spans, "compact_last", compact.last_phase, width)
	end

	add_kv(lines, spans, "cost", cost and cost > 0 and smelt.text.format_cost(cost) or "0", width)
	add_kv(lines, spans, "pricing", pricing.source, width)

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
	add_kv(lines, spans, "tokens", #tok_parts > 0 and table.concat(tok_parts, " ") or "nil", width)
	add_kv(lines, spans, "cache_hit", tokens.cache_hit_ratio and string.format("%.1f%%", tokens.cache_hit_ratio * 100) or "nil", width)

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
	add_kv(lines, spans, "sampling", #cfg_parts > 0 and table.concat(cfg_parts, " ") or "defaults", width)

	if mcfg.thinking_budgets then
		local b = mcfg.thinking_budgets
		add_kv(lines, spans, "thinking", string.format("low=%s med=%s high=%s max=%s", smelt.text.format_tokens(b.low), smelt.text.format_tokens(b.medium), smelt.text.format_tokens(b.high), smelt.text.format_tokens(b.max)), width)
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
		add_kv(lines, spans, "cost_override", table.concat(cost_parts, " "), width)
	end

	add_kv(lines, spans, "turns", tostring(#turns), width)
	add_kv(lines, spans, "messages", tostring(#messages), width)
	add_kv(lines, spans, "work_state", work_state, width)

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
	local ok, lines, spans = pcall(compose_lines, win)
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
	state.win = smelt.win.new(state.buf, {
		name = "debug_panel.win",
		surface = "readonly_text",
		hide_cursor = true,
		vim_enabled = smelt.settings.vim and true or false,
	})
	state.win:key("esc", close)
	state.win:key("q", close)
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
	state.win:focus()
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
