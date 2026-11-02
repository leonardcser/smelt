-- Compacts older history while preserving a live recent suffix.
local recovery_open = false
local pending_recovery

local function offer_context_recovery()
	if recovery_open then return end
	recovery_open = true
	local lifecycle = smelt.lifecycle.guard({ "session" })
	smelt.spawn(function()
		local ok, picked = pcall(function()
			local explanation = smelt.dialog.markdown(
				"This model cannot compact the current history.\n\n"
				.. "Archive and continue saves the full history locally and starts a fresh session "
				.. "on the selected model. The old session stays available."
			)
			local choices = smelt.dialog.menu({
				"Keep session",
				"Retry compaction",
				"Archive and continue",
			})
			return smelt.dialog.open({
				title = "context limit",
				blocks_agent = true,
				focus = choices,
				on_close = function() recovery_open = false end,
				panels = { { leaf = explanation, height = "fit" } },
				bottom_panels = { { leaf = choices, height = "fit" } },
			})
		end)
		recovery_open = false
		if not ok or not lifecycle:alive() or not picked then return end
		if picked.index == 2 then
			smelt.cmd.run("compact")
		elseif picked.index == 3 then
			local archived, err = pcall(__smelt_internal.session.__continue_archived, require("smelt.goal").status_text() or "")
			if not archived and not smelt.task.is_cancelled(err) then
				smelt.notify.error(tostring(err))
			end
		end
	end)
end

local function context_failure()
	-- A dialog spawned by the failing request is turn-scoped and would be
	-- cancelled when the engine aborts. Offer recovery after that turn ends.
	if smelt.work.guard().turn_id then
		pending_recovery = smelt.lifecycle.guard({ "session" })
	else
		offer_context_recovery()
	end
end

smelt.events.on("turn_end", function(event)
	local pending = pending_recovery
	pending_recovery = nil
	if (pending and pending:alive()) or event.error_kind == "context_window" then
		offer_context_recovery()
	end
end)
smelt.events.on("session_started", function()
	pending_recovery = nil
	recovery_open = false
end)

local compact = require("smelt.compact").new({
	summary_prefix = smelt.engine.summary_prefix(),
	state = smelt.state.get("compact"),
	settings = smelt.settings,
	truncate = smelt.text.truncate,
	is_cancelled = smelt.task.is_cancelled,
	notify = smelt.notify,
	log = smelt.log.info,
	context_failure = context_failure,
	ask = smelt.engine.ask_inherited,
	-- A summary carries the whole conversation forward, so it defaults to the
	-- main model rather than the cheaper one other helpers use.
	preferred_model = function()
		return smelt.model.preferred("compact") or smelt.model.current()
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
