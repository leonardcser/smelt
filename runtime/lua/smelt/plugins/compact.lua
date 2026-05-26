-- Compaction plugin. Owns the /compact command and the post-turn
-- auto-compact subscription. When the context window is filling up, it
-- runs inherited-session summarisation over the older history with a
-- fixed structured prompt while preserving a live suffix verbatim.
--
-- Compaction is visible to the user via `smelt.work.busy("compacting")`
-- and emits structured `compaction` log events with before/after token
-- counts.

-- Task instruction appended as the FINAL user message of the summariser
-- request. Everything before it (system, tools, prior messages) mirrors
-- the main session, so the request hits the same Anthropic prefix cache
-- slot. Only this trailing instruction is fresh on each compaction.
local SUMMARY_TASK = [[
The conversation above is becoming long. Stop the current task and instead produce a CONTEXT CHECKPOINT COMPACTION: a structured handoff summary that another instance of yourself will read to resume the task without losing critical context.

Reply in this exact Markdown structure. Omit a section only if the conversation truly contains nothing for it; never invent details. Respond with ONLY the Markdown document — no preamble, no apology. Under no circumstances use tools; any tool call will be denied and you must answer with the Markdown summary only.

# Goal
What the user is trying to accomplish (one or two sentences).

# Constraints
Hard limits, style rules, environment facts, anything the next instance must respect.

# Progress
What has already been done. Concrete, specific, in completion order.

# Decisions
Choices that were made and the rationale, when the rationale matters for what comes next.

# Next steps
Ordered, concrete actions the next instance should take.

# Critical context
File contents, error messages, command output, exact identifiers, etc. that the next instance will need verbatim. Quote precisely.

# Relevant files
Bullet list of file paths that were touched or are about to be touched, with a one-line note on why each matters.
]]

local SUMMARY_PREFIX = smelt.engine.summary_prefix()

local INSTRUCTIONS_PREAMBLE = "The user has asked you to pay special attention to the following when summarizing:"

-- How many times to re-issue the summarizer call when the model returns
-- an empty response before giving up.
local MAX_EMPTY_RETRIES = 2

-- Tool use is forbidden during compaction even though the inherited
-- request keeps the live tool schema for KV-cache reuse. If the model
-- calls a tool anyway, we append denial tool results and retry the same
-- boundary. A repeated tool-call response restarts that boundary once
-- from the original request shape before giving up.
local MAX_TOOL_CALL_RESTARTS = 2

local TOOL_DENIED_MESSAGE = "Tool use is not allowed during compaction. Respond with text only."

-- Circuit breaker. After this many consecutive failed compactions, the
-- plugin stops auto-firing for the rest of the session to avoid burning
-- tokens in a loop. /compact still works (manual override).
local MAX_CONSECUTIVE_FAILURES = 3
local consecutive_failures = 0

-- ── helpers ────────────────────────────────────────────────────────────

-- Compose the trailing user message. Folds optional per-call instructions
-- into the structured-summary spec.
local function build_summary_task(instructions)
	local task = SUMMARY_TASK:gsub("^%s+", ""):gsub("%s+$", "")
	if instructions then
		local extra = instructions:gsub("^%s+", ""):gsub("%s+$", "")
		if extra ~= "" then
			task = task .. "\n\n" .. INSTRUCTIONS_PREAMBLE .. "\n" .. extra
		end
	end
	return task
end

local function clone_messages(messages)
	local out = {}
	for i, msg in ipairs(messages or {}) do
		out[i] = msg
	end
	return out
end

local function append_tool_denials(messages, assistant_response)
	local out = clone_messages(messages)
	table.insert(out, assistant_response)
	for _, call in ipairs(assistant_response.tool_calls or {}) do
		table.insert(out, {
			role = "tool",
			tool_call_id = call.id,
			content = TOOL_DENIED_MESSAGE,
			is_error = true,
		})
	end
	return out
end

-- Async summarizer primitive shared by /compact and post-turn auto-compact.
-- Uses `ask_inherited` so the request shares the main turn's
-- system prompt and tool list. `history` may be a strict prefix of the
-- live model-visible conversation; when omitted the full live history is
-- inherited.
local function summarize_messages_with_handle(history, instructions, handle, done)
	if not history or #history == 0 then
		done(nil)
		return
	end
	local task = build_summary_task(instructions)
	local owns_handle = false
	if not handle then
		handle = smelt.work.busy("compacting")
		owns_handle = true
	end
	local empty_retries = 0
	local boundary_restarts = 0
	local base_messages = clone_messages(history)
	table.insert(base_messages, { role = "user", content = task })

	local function finish(summary, err)
		if owns_handle then
			handle:remove()
		end
		done(summary, err)
	end

	local function send(messages, retrying_after_tool_denial)
		smelt.engine.ask_inherited({
			messages = messages,
			model = smelt.model.preferred("compact"),
			on_response = function(response, err)
				if err then
					if smelt.task.is_cancelled(err) then
						finish(nil, err)
						return
					end
					smelt.notify.error("compaction failed: " .. err.message)
					finish(nil, err)
					return
				end
				if response and response.tool_calls and #response.tool_calls > 0 then
					if not retrying_after_tool_denial then
						send(append_tool_denials(messages, response), true)
						return
					end
					if boundary_restarts < MAX_TOOL_CALL_RESTARTS then
						boundary_restarts = boundary_restarts + 1
						send(base_messages, false)
						return
					end
					finish(nil, {
						kind = "invalid_response",
						message = "model kept requesting tools during compaction",
					})
					return
				end
				local summary = response and response.content or nil
				if not summary or summary == "" then
					if empty_retries < MAX_EMPTY_RETRIES then
						empty_retries = empty_retries + 1
						send(base_messages, false)
						return
					end
					smelt.notify.error("compaction failed: empty summary after retries")
					finish(nil, nil)
					return
				end
				finish(summary, nil)
			end,
		})
	end

	send(base_messages, false)
end

local function build_message_groups(history)
	local groups = {}
	local i = 1
	while i <= #history do
		local msg = history[i]
		if msg.role == "assistant" then
			local j = i
			if msg.tool_calls and #msg.tool_calls > 0 then
				j = i + 1
				while j <= #history and history[j].role == "tool" do
					j = j + 1
				end
				j = j - 1
			end
			table.insert(groups, { start_idx = i, end_idx = j })
			i = j + 1
		else
			table.insert(groups, { start_idx = i, end_idx = i })
			i = i + 1
		end
	end
	return groups
end

local function slice_group_prefix(history, groups, last_group)
	local out = {}
	if last_group <= 0 then
		return out
	end
	for group_idx = 1, last_group do
		local group = groups[group_idx]
		for msg_idx = group.start_idx, group.end_idx do
			table.insert(out, history[msg_idx])
		end
	end
	return out
end

local function suffix_first_live_message_index(history, groups, suffix_start_group)
	if suffix_start_group > #groups then
		return #history
	end
	return groups[suffix_start_group].start_idx - 1
end

local function checkpointed_messages_from_boundary(history, summary, first_live_message_index)
	local out = {
		{ role = "user", content = SUMMARY_PREFIX .. "\n" .. summary },
	}
	for i = first_live_message_index + 1, #history do
		table.insert(out, history[i])
	end
	return out
end

local function summarize_by_group_boundary(history, instructions, done)
	if not history or #history == 0 then
		done(nil)
		return
	end

	local groups = build_message_groups(history)
	if #groups == 0 then
		done(nil)
		return
	end

	local min_postponed_groups = math.max(0, math.floor(smelt.settings.compact_keep_recent_groups or 1))
	local suffix_start_group = math.max(1, (#groups + 1) - math.min(min_postponed_groups, #groups))
	local handle = smelt.work.busy("compacting")
	local finished = false

	local function finish(summary, err, first_live_message_index)
		if finished then
			return
		end
		finished = true
		handle:remove()
		done(summary, err, first_live_message_index)
	end

	local function attempt()
		local prefix_last_group = suffix_start_group - 1
		if prefix_last_group <= 0 then
			finish(nil)
			return
		end

		local prefix_messages = slice_group_prefix(history, groups, prefix_last_group)
		summarize_messages_with_handle(prefix_messages, instructions, handle, function(summary, err)
			if summary then
				finish(summary, nil, suffix_first_live_message_index(history, groups, suffix_start_group))
				return
			end
			if smelt.task.is_cancelled(err) then
				finish(nil, err)
				return
			end
			if err and err.kind == "context_window" then
				if prefix_last_group > 1 then
					suffix_start_group = suffix_start_group - 1
					attempt()
					return
				end
				finish(nil, err)
				return
			end
			finish(nil, err)
		end)
	end

	attempt()
end

local function emit_event(phase, before_tokens, after_tokens, extra)
	local data = { phase = phase }
	if before_tokens then
		data.before_tokens = before_tokens
	end
	if after_tokens then
		data.after_tokens = after_tokens
	end
	if before_tokens and after_tokens then
		data.saved_tokens = math.max(0, before_tokens - after_tokens)
	end
	if extra then
		for k, v in pairs(extra) do
			data[k] = v
		end
	end
	smelt.log.info("compaction", data)
end

local function run_compact(opts)
	local history = smelt.session.model_messages()
	if not history or #history == 0 then
		smelt.notify.error("nothing to compact")
		return
	end
	-- Use the provider's actual prompt-token count.  If no turn has
	-- completed yet (context_tokens is nil) we skip rather than guess.
	local before_tokens = smelt.session.context_tokens()
	if not before_tokens then
		smelt.notify.error("no token usage available yet; try again after the next turn completes")
		return
	end

	-- Summarise the original history. `ask_inherited` keeps the request
	-- prefix byte-identical to the main turn for cache reuse.
	summarize_by_group_boundary(history, opts and opts.instructions, function(summary, err, first_live_message_index)
		if smelt.task.is_cancelled(err) then
			return
		end
		if not summary then
			consecutive_failures = consecutive_failures + 1
			return
		end
		consecutive_failures = 0
		smelt.session.checkpoint({
			kind = "compaction",
			summary = summary,
			first_live_message_index = first_live_message_index,
			tokens_before = before_tokens,
		})
		emit_event("summarize", before_tokens, nil, {
			first_live_message_index = first_live_message_index,
		})
	end)
end

local function auto_compact_due(tokens)
	if not smelt.settings.auto_compact then
		return false
	end
	if consecutive_failures >= MAX_CONSECUTIVE_FAILURES then
		return false
	end
	local window = smelt.session.context_window()
	if not window then
		return false
	end
	local threshold = smelt.settings.compact_threshold
	if not (threshold > 0 and threshold <= 1) then
		threshold = 0.80
	end
	if not tokens then
		return false
	end
	return tokens >= window * threshold
end

local function compact_live_session(before_tokens, phase, done)
	local history = smelt.session.model_messages()
	if not history or #history == 0 then
		done(nil)
		return
	end
	summarize_by_group_boundary(history, nil, function(summary, err, first_live_message_index)
		if smelt.task.is_cancelled(err) then
			done(nil)
			return
		end
		if not summary then
			consecutive_failures = consecutive_failures + 1
			done(nil)
			return
		end
		consecutive_failures = 0
		local model_messages = smelt.session.checkpoint({
			kind = "compaction",
			summary = summary,
			first_live_message_index = first_live_message_index,
			tokens_before = before_tokens,
		})
		if not model_messages then
			done(nil)
			return
		end
		emit_event(phase, before_tokens, nil, {
			first_live_message_index = first_live_message_index,
		})
		done(model_messages)
	end)
end

-- ── /compact command ──────────────────────────────────────────────────

smelt.cmd.register("compact", function(arg)
	consecutive_failures = 0
	run_compact({ instructions = arg, inject_recent_user_messages = false })
end, { desc = "compact conversation history", args = { "<instructions>" }, while_busy = false })

-- ── pre-request auto-compaction ───────────────────────────────────────
--
-- Engine invokes this immediately before sending a model request. This
-- is the single normal auto-compact path: it uses the active-context
-- estimate for the request the engine is actually about to send, anchored
-- to the last provider-reported context size when available.

smelt.engine.on_prepare_request(function(request, reply)
	local estimated_tokens = request and (request.estimated_context_tokens or request.estimated_tokens)
	if not auto_compact_due(estimated_tokens) then
		reply(nil)
		return
	end
	local estimate = request and request.context_estimate
	emit_event("trigger-pre-request", estimated_tokens, nil, {
		estimate_source = estimate and estimate.source or nil,
		provider_context_tokens = estimate and estimate.provider_context_tokens or nil,
		estimated_delta_tokens = estimate and estimate.estimated_delta_tokens or nil,
		snapshot_history_len = estimate and estimate.latest_snapshot_history_len or nil,
		current_history_len = estimate and estimate.current_history_len or nil,
	})
	compact_live_session(estimated_tokens, "summarize-pre-request", function(messages)
		reply(messages)
	end)
end)

-- ── mid-turn recovery hook ────────────────────────────────────────────
--
-- Engine invokes this when a provider returns a context-window error
-- mid-turn. Honours `settings.auto_compact`: when disabled, the hook
-- calls `reply(nil)` immediately so the engine aborts the turn with the
-- standard error. When enabled, reruns the same group-boundary compaction
-- algorithm against the failing request snapshot and hands the engine a
-- shortened replacement request: compaction summary + preserved live suffix.

smelt.engine.on_context_limit(function(messages, reply)
	if not smelt.settings.auto_compact then
		reply(nil)
		return
	end

	summarize_by_group_boundary(messages, nil, function(summary, err, first_live_message_index)
		if smelt.task.is_cancelled(err) then
			reply(nil)
			return
		end
		if not summary then
			consecutive_failures = consecutive_failures + 1
			reply(nil)
			return
		end
		consecutive_failures = 0
		local replacement = checkpointed_messages_from_boundary(messages, summary, first_live_message_index)
		emit_event("summarize-recovery", 0, 0, {
			first_live_message_index = first_live_message_index,
		})
		reply(replacement)
	end)
end)
