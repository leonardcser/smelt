-- Compacts older history while preserving a live recent suffix.
local compact = require("smelt.compact").new({
	summary_prefix = smelt.engine.summary_prefix(),
	state = smelt.state.get("compact"),
	settings = smelt.settings,
	truncate = smelt.text.truncate,
	is_cancelled = smelt.task.is_cancelled,
	notify = smelt.notify,
	log = smelt.log.info,
	ask = smelt.engine.ask_inherited,
	preferred_model = function()
		return smelt.model.preferred("compact")
	end,
	messages = smelt.session.model_messages,
	context_tokens = smelt.session.context_tokens,
	context_window = smelt.session.context_window,
	checkpoint = smelt.session.checkpoint,
	guard = smelt.work.guard,
	guard_current = smelt.work.guard_current,
	preview = __smelt_internal.transcript._set_compaction_preview,
	begin_work = function()
		local handle = __smelt_internal.work._context_recalculation("compacting")
		local guard = smelt.work.guard()
		local lifecycle = smelt.lifecycle.guard({ "session", "history" }):latest("compaction")
		return {
			alive = function()
				return handle:alive() and lifecycle:alive() and smelt.work.guard_current(guard)
			end,
			remove = function()
				handle:remove()
			end,
		}
	end,
})

smelt.cmd.register("compact", compact.manual, {
	desc = "compact conversation history",
	args = { "<instructions>" },
	busy = "reject",
})
smelt.engine.on_prepare_request(compact.prepare_request)
smelt.engine.on_context_limit(compact.context_limit)
