-- Compaction plugin. Owns the /compact command and the post-turn
-- auto-compact subscription. Runs a two-phase pass when the context window
-- is filling up:
--
--   Phase A (prune): cheap, no LLM call. Replaces the bodies of old tool
--                    results with elision markers so a flood of large
--                    tool outputs doesn't force a summarisation round-trip.
--   Phase B (summarise): if phase A doesn't reclaim enough, runs an LLM
--                    summarisation over the older history with a fixed
--                    structured prompt. Recent turns are preserved verbatim.
--
-- Both phases are visible to the user via `smelt.work.busy("compacting")`
-- and emit structured `compaction` log events with before/after token counts.

-- Task instruction appended as the FINAL user message of the summariser
-- request. Everything before it (system, tools, prior messages) mirrors
-- the main session, so the request hits the same Anthropic prefix cache
-- slot. Only this trailing instruction is fresh on each compaction.
local SUMMARY_TASK = [[
The conversation above is becoming long. Stop the current task and instead produce a CONTEXT CHECKPOINT COMPACTION: a structured handoff summary that another instance of yourself will read to resume the task without losing critical context.

Reply in this exact Markdown structure. Omit a section only if the conversation truly contains nothing for it; never invent details. Respond with ONLY the Markdown document — no preamble, no apology, no tool calls.

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

-- Per-message byte cap when flattening history for the summarizer. Caps
-- runaway tool outputs from blowing up the summariser's own context.
local MAX_STRINGIFIED_MESSAGE_BYTES = 8000
local TRUNCATION_SUFFIX = "\n…[truncated for compaction]"

-- Phase A: replace tool-result bodies older than this byte budget with
-- an elision marker. Keeps the most recent `PHASE_A_KEEP_RECENT_BYTES`
-- of tool content verbatim; everything before it gets `<elided …>`.
local PHASE_A_KEEP_RECENT_BYTES = 40000

-- How many times to re-issue the summarizer call when the model returns
-- an empty response before giving up.
local MAX_EMPTY_RETRIES = 2

-- Circuit breaker. After this many consecutive failed compactions, the
-- plugin stops auto-firing for the rest of the session to avoid burning
-- tokens in a loop. /compact still works (manual override).
local MAX_CONSECUTIVE_FAILURES = 3
local consecutive_failures = 0

-- ── helpers ────────────────────────────────────────────────────────────

local function is_summary_user(msg)
  if msg.role ~= "user" or not msg.content then return false end
  local trimmed = msg.content:gsub("^%s+", ""):gsub("%s+$", "")
  return trimmed:sub(1, #SUMMARY_PREFIX) == SUMMARY_PREFIX
end

-- Strip image attachments from a flat copy of the history. The summariser
-- model only needs the textual conversation; pulling images into the
-- summariser turn can itself overflow the context window on image-heavy
-- sessions. Pure function, returns a new list.
local function strip_images(history)
  local out = {}
  for _, m in ipairs(history) do
    local copy = {}
    for k, v in pairs(m) do copy[k] = v end
    if type(copy.content) == "table" then
      local cleaned = {}
      for _, part in ipairs(copy.content) do
        if not (part.type == "image" or part.type == "image_url") then
          table.insert(cleaned, part)
        end
      end
      copy.content = cleaned
    end
    table.insert(out, copy)
  end
  return out
end

-- Phase A: prune old tool-result bodies. Walks the history from the tail
-- backwards counting tool-result bytes; everything past
-- `PHASE_A_KEEP_RECENT_BYTES` of *recent* tool content is replaced with an
-- elision marker. Returns the new history plus the number of bytes
-- elided (for telemetry).
local function prune_old_tool_results(history)
  local kept_bytes = 0
  local elided_bytes = 0
  local new_history = {}
  -- First pass: pick the cut index. Walk backwards counting bytes of
  -- tool-result content until we exceed the keep-recent budget.
  local cut_index = 0
  for i = #history, 1, -1 do
    local m = history[i]
    if m.role == "tool" and type(m.content) == "string" then
      kept_bytes = kept_bytes + #m.content
      if kept_bytes > PHASE_A_KEEP_RECENT_BYTES then
        cut_index = i
        break
      end
    end
  end
  -- Second pass: rewrite earlier tool-result bodies.
  for i, m in ipairs(history) do
    if i <= cut_index and m.role == "tool" and type(m.content) == "string"
        and not m.content:match("^<elided ") then
      local copy = {}
      for k, v in pairs(m) do copy[k] = v end
      elided_bytes = elided_bytes + #m.content
      copy.content = string.format(
        "<elided %d bytes of tool output during compaction>", #m.content)
      table.insert(new_history, copy)
    else
      table.insert(new_history, m)
    end
  end
  return new_history, elided_bytes
end

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

-- Build a trim-friendly summarizer message. `smelt.engine.ask_with_trim`
-- removes whole entries from the front on context-window errors, matching
-- Codex's "drop oldest history item and retry" compaction behavior.
local function summarizer_message(m)
  local label, body
  if m.role == "system" then
    return nil
  elseif m.role == "user" then
    label, body = "User", m.content or ""
  elseif m.role == "tool" then
    label, body = "ToolResult", m.content or ""
  elseif m.role == "assistant" then
    label = "Assistant"
    local pieces = {}
    if m.reasoning_content and m.reasoning_content:gsub("%s+", "") ~= "" then
      table.insert(pieces, "[thinking]\n" .. m.reasoning_content:gsub("^%s+", ""):gsub("%s+$", "") .. "\n")
    end
    if m.content and m.content ~= "" then
      table.insert(pieces, m.content)
    end
    if m.tool_calls then
      for _, call in ipairs(m.tool_calls) do
        local fn = call.function_call or call["function"] or {}
        local name = fn.name or call.name or "?"
        local args = fn.arguments or call.arguments or ""
        table.insert(pieces, "[tool_call] " .. name .. "(" .. args .. ")")
      end
    end
    body = table.concat(pieces, "\n")
  else
    label, body = nil, nil
  end
  if not (label and body) then return nil end
  local trimmed = body:gsub("^%s+", ""):gsub("%s+$", "")
  if trimmed == "" then return nil end
  local capped = smelt.text.truncate(trimmed, MAX_STRINGIFIED_MESSAGE_BYTES, TRUNCATION_SUFFIX)
  local role = "user"
  if m.role == "assistant" then role = "assistant" end
  return { role = role, content = label .. ": " .. capped }
end

local function build_summarizer_messages(history, instructions)
  local messages = {}
  for _, m in ipairs(history or {}) do
    local msg = summarizer_message(m)
    if msg then table.insert(messages, msg) end
  end
  table.insert(messages, {
    role = "user",
    content = build_summary_task(instructions),
  })
  return messages
end

-- Legacy fallback summariser: flattens the supplied history into a single
-- trim-friendly message list and sends it with `ask_with_trim`. Used by
-- the mid-turn recovery hook (where the live session may not be settled)
-- and as the overflow fallback for the inheriting path.
local function summarize_flat_with_handle(history, instructions, handle, done)
  if not history or #history == 0 then
    done(nil)
    return
  end
  local cleaned = strip_images(history)
  local messages = build_summarizer_messages(cleaned, instructions)
  local owns_handle = false
  if not handle then
    handle = smelt.work.busy("compacting")
    owns_handle = true
  end
  local empty_retries = 0

  -- Reuse the main session's system prompt so the system block hits the
  -- same Anthropic cache slot. Tools are still empty here (the flat path
  -- can't send tools without breaking the trim wrapper), so the longer
  -- prefix won't hit — but the system slot alone is the bulk of the win.
  local system = smelt.session.system()
  if not system or system == "" then
    system = "You are performing a context-checkpoint compaction. Produce ONLY the requested Markdown summary."
  end

  local function send()
    smelt.engine.ask_with_trim({
      system      = system,
      messages    = messages,
      model       = smelt.model.preferred("compact"),
      max_trims   = 20,
      on_response = function(summary, err)
        if err then
          if owns_handle then handle:remove() end
          if smelt.task.is_cancelled(err) then
            done(nil, err)
            return
          end
          smelt.notify.error("compaction failed: " .. err.message)
          done(nil, err)
          return
        end
        if not summary or summary == "" then
          if empty_retries < MAX_EMPTY_RETRIES then
            empty_retries = empty_retries + 1
            send()
            return
          end
          if owns_handle then handle:remove() end
          smelt.notify.error("compaction failed: empty summary after retries")
          done(nil, nil)
          return
        end
        if owns_handle then handle:remove() end
        done(summary, nil)
      end,
    })
  end

  send()
end

local function summarize_flat(history, instructions, done)
  summarize_flat_with_handle(history, instructions, nil, done)
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

  local function send()
    smelt.engine.ask_inherited({
      messages    = history,
      question    = task,
      model       = smelt.model.preferred("compact"),
      on_response = function(summary, err)
        if err then
          if owns_handle then handle:remove() end
          if smelt.task.is_cancelled(err) then
            done(nil, err)
            return
          end
          smelt.notify.error("compaction failed: " .. err.message)
          done(nil, err)
          return
        end
        if not summary or summary == "" then
          if empty_retries < MAX_EMPTY_RETRIES then
            empty_retries = empty_retries + 1
            send()
            return
          end
          if owns_handle then handle:remove() end
          smelt.notify.error("compaction failed: empty summary after retries")
          done(nil, nil)
          return
        end
        if owns_handle then handle:remove() end
        done(summary, nil)
      end,
    })
  end

  send()
end

local function summarize_messages(history, instructions, done)
  summarize_messages_with_handle(history, instructions, nil, done)
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
  if last_group <= 0 then return out end
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
    if finished then return end
    finished = true
    handle:remove()
    done(summary, err, first_live_message_index)
  end

  local function fallback_trimmed(prefix_messages)
    summarize_flat_with_handle(prefix_messages, instructions, handle, function(summary, err)
      if not summary or smelt.task.is_cancelled(err) then
        finish(nil, err)
        return
      end
      finish(summary, nil, suffix_first_live_message_index(history, groups, suffix_start_group))
    end)
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
        fallback_trimmed(prefix_messages)
        return
      end
      finish(nil, err)
    end)
  end

  attempt()
end

local function emit_event(phase, before_tokens, after_tokens, extra)
  local data = { phase = phase }
  if before_tokens then data.before_tokens = before_tokens end
  if after_tokens then data.after_tokens = after_tokens end
  if before_tokens and after_tokens then
    data.saved_tokens = math.max(0, before_tokens - after_tokens)
  end
  if extra then
    for k, v in pairs(extra) do data[k] = v end
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

  -- Phase B only: summarise the original history. `ask_inherited`
  -- keeps the request prefix byte-identical to the main turn for cache
  -- reuse.  Phase A (prune) is reserved for mid-turn recovery where we
  -- have no other option.
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
  if not smelt.settings.auto_compact then return false end
  if consecutive_failures >= MAX_CONSECUTIVE_FAILURES then return false end
  local window = smelt.session.context_window()
  if not window then return false end
  local threshold = smelt.settings.compact_threshold
  if not (threshold > 0 and threshold <= 1) then threshold = 0.80 end
  if not tokens then return false end
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
-- standard error. When enabled, summarises the conversation and hands
-- the engine a one-message replacement carrying the summary, letting
-- the turn continue seamlessly without a user-visible failure.

smelt.engine.on_context_limit(function(messages, reply)
  if not smelt.settings.auto_compact then
    reply(nil)
    return
  end

  -- Cascading fallback: try phase A first (cheap, no LLM). If pruning
  -- alone reclaims enough space the recovery returns the pruned list
  -- without a summarisation round-trip.
  local pruned, elided_bytes = prune_old_tool_results(messages)
  if elided_bytes > 0 then
    emit_event("prune-recovery", 0, 0, { elided_bytes = elided_bytes })
    -- Try with just the prune. If the engine still 413s, this hook will
    -- fire again and we'll fall through to summarisation.
    reply(pruned)
    return
  end

  -- Mid-turn recovery uses the flat path: the engine hands us a `messages`
  -- snapshot that doesn't match `smelt.session.messages()`, so we can't
  -- use `ask_inherited` cleanly. Cache reuse is worth less here anyway — the
  -- turn is already failing.
  summarize_flat(messages, nil, function(summary, err)
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
    local replacement = { { role = "user", content = SUMMARY_PREFIX .. "\n" .. summary } }
    emit_event("summarize-recovery", 0, 0)
    reply(replacement)
  end)
end)
