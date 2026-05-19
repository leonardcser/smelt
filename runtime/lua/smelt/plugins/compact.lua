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
-- Both phases are visible to the user via `smelt.spinner.busy("compacting")`
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

local SUMMARY_PREFIX = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:"

local INSTRUCTIONS_PREAMBLE = "The user has asked you to pay special attention to the following when summarizing:"

-- Per-message byte cap when flattening history for the summarizer. Caps
-- runaway tool outputs from blowing up the summariser's own context.
local MAX_STRINGIFIED_MESSAGE_BYTES = 8000
local TRUNCATION_SUFFIX = "\n…[truncated for compaction]"

-- Soft token cap on user messages carried forward when injecting recent
-- user turns alongside the summary.
local COMPACT_USER_MESSAGE_MAX_TOKENS = 20000

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

local function approx_tokens(s) return math.ceil(#s / 4) end

local function approx_history_tokens(history)
  local total = 0
  for _, m in ipairs(history) do
    if type(m.content) == "string" then total = total + approx_tokens(m.content) end
    if m.reasoning_content then total = total + approx_tokens(m.reasoning_content) end
    if m.tool_calls then
      for _, c in ipairs(m.tool_calls) do
        local fn = c["function"] or c.function_call or {}
        if fn.arguments then total = total + approx_tokens(fn.arguments) end
      end
    end
  end
  return total
end

local function is_summary_user(msg)
  if msg.role ~= "user" or not msg.content then return false end
  local trimmed = msg.content:gsub("^%s+", ""):gsub("%s+$", "")
  return trimmed:sub(1, #SUMMARY_PREFIX) == SUMMARY_PREFIX
end

local function collect_user_messages(history)
  local out = {}
  for _, m in ipairs(history) do
    if m.role == "user" and m.content and m.content ~= "" and not is_summary_user(m) then
      table.insert(out, m.content)
    end
  end
  return out
end

local function select_recent(user_msgs, max_tokens)
  if max_tokens <= 0 or #user_msgs == 0 then return {} end
  local remaining = max_tokens
  local picked = {}
  for i = #user_msgs, 1, -1 do
    if remaining <= 0 then break end
    local msg = user_msgs[i]
    local tokens = approx_tokens(msg)
    if tokens <= remaining then
      table.insert(picked, 1, msg)
      remaining = remaining - tokens
    else
      table.insert(picked, 1, smelt.text.truncate(msg, remaining * 4, TRUNCATION_SUFFIX))
      break
    end
  end
  return picked
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

local function build_replacement(history, summary, inject_recent_user_messages)
  local out = {}
  if inject_recent_user_messages then
    local users = collect_user_messages(history)
    for _, text in ipairs(select_recent(users, COMPACT_USER_MESSAGE_MAX_TOKENS)) do
      table.insert(out, { role = "user", content = text })
    end
  end
  local body = summary
  if not body or body:gsub("%s+", "") == "" then
    body = "(no summary available)"
  end
  table.insert(out, { role = "user", content = SUMMARY_PREFIX .. "\n" .. body })
  return out
end

-- Build a single flattened transcript string the summarizer model sees
-- as `user` content. Labels by role, trims, drops blanks, caps each
-- entry's bytes so a single oversized tool result can't blow the
-- summariser's own context budget.
local function stringify_conversation(history)
  local parts = {}
  for _, m in ipairs(history) do
    local label, body
    if m.role == "system" then
      label, body = "System", m.content or ""
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
    if label and body then
      local trimmed = body:gsub("^%s+", ""):gsub("%s+$", "")
      if trimmed ~= "" then
        local capped = smelt.text.truncate(trimmed, MAX_STRINGIFIED_MESSAGE_BYTES, TRUNCATION_SUFFIX)
        table.insert(parts, label .. ": " .. capped)
      end
    end
  end
  return table.concat(parts, "\n\n")
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

-- Legacy fallback summariser: flattens the supplied history into a single
-- user message and sends it with `ask_with_trim`. Used by the mid-turn
-- recovery hook (where the live session may not be settled) and as the
-- overflow fallback for the inheriting path.
local function summarize_flat(history, instructions, done)
  if not history or #history == 0 then
    done(nil)
    return
  end
  local cleaned = strip_images(history)
  local task = build_summary_task(instructions)
  local user_text = task .. "\n\nConversation to summarize:\n\n" .. stringify_conversation(cleaned)
  local handle = smelt.spinner.busy("compacting")
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
      messages    = { { role = "user", content = user_text } },
      model       = smelt.model.preferred("compact"),
      max_trims   = 20,
      on_response = function(summary, err)
        if err then
          handle:remove()
          smelt.notify.error("compaction failed: " .. err.message)
          done(nil)
          return
        end
        if not summary or summary == "" then
          if empty_retries < MAX_EMPTY_RETRIES then
            empty_retries = empty_retries + 1
            send()
            return
          end
          handle:remove()
          smelt.notify.error("compaction failed: empty summary after retries")
          done(nil)
          return
        end
        handle:remove()
        done(summary)
      end,
    })
  end

  send()
end

-- Async summarizer primitive shared by /compact and post-turn auto-compact.
-- Uses `inherit_session = true` so the request prefix matches the main
-- turn byte-for-byte and reuses its Anthropic prompt-cache slot. Falls
-- back to the flat stringify path on context-window overflow.
local function summarize(history, instructions, done)
  if not history or #history == 0 then
    done(nil)
    return
  end
  local task = build_summary_task(instructions)
  local handle = smelt.spinner.busy("compacting")
  local empty_retries = 0

  local function send()
    smelt.engine.ask({
      inherit_session = true,
      question        = task,
      model           = smelt.model.preferred("compact"),
      on_response     = function(summary, err)
        if err then
          if err.kind == "context_window" then
            -- Inherited session overflowed the summariser window. Fall back
            -- to the flat stringify path, which trims aggressively.
            handle:remove()
            summarize_flat(history, instructions, done)
            return
          end
          handle:remove()
          smelt.notify.error("compaction failed: " .. err.message)
          done(nil)
          return
        end
        if not summary or summary == "" then
          if empty_retries < MAX_EMPTY_RETRIES then
            empty_retries = empty_retries + 1
            send()
            return
          end
          handle:remove()
          smelt.notify.error("compaction failed: empty summary after retries")
          done(nil)
          return
        end
        handle:remove()
        done(summary)
      end,
    })
  end

  send()
end

local function emit_event(phase, before_tokens, after_tokens, extra)
  local data = {
    phase = phase,
    before_tokens = before_tokens,
    after_tokens = after_tokens,
    saved_tokens = math.max(0, before_tokens - after_tokens),
  }
  if extra then
    for k, v in pairs(extra) do data[k] = v end
  end
  smelt.log.info("compaction", data)
end

-- Threshold (fraction of context window) below which Phase A's prune is
-- considered "enough" — we commit the prune and skip the summarisation
-- LLM call. Above this we discard the prune so Phase B can inherit the
-- un-mutated session and reuse the main turn's prompt-cache slot.
local PHASE_A_SUFFICIENT_THRESHOLD = 0.60

local function run_compact(opts)
  local history = smelt.session.messages()
  if not history or #history == 0 then
    smelt.notify.error("nothing to compact")
    return
  end
  local before_tokens = approx_history_tokens(history)

  -- Phase A: speculatively prune older tool-result bodies. Commit only
  -- when prune alone brings tokens comfortably under threshold; otherwise
  -- discard so the Phase B summariser inherits the original session and
  -- hits the main-turn cache. Applying the prune before summarising
  -- would rewrite early-prefix messages and bust that cache.
  local pruned, elided_bytes = prune_old_tool_results(history)
  local window = smelt.session.context_window()
  if elided_bytes > 0 and window then
    local pruned_tokens = approx_history_tokens(pruned)
    if pruned_tokens < window * PHASE_A_SUFFICIENT_THRESHOLD then
      smelt.session.messages(pruned)
      emit_event("prune", before_tokens, pruned_tokens, { elided_bytes = elided_bytes })
      return
    end
  end

  -- Phase B: summarise the original history. `inherit_session` keeps the
  -- request prefix byte-identical to the main turn for cache reuse.
  summarize(history, opts and opts.instructions, function(summary)
    if not summary then
      consecutive_failures = consecutive_failures + 1
      return
    end
    consecutive_failures = 0
    local replacement = build_replacement(
      history,
      summary,
      opts and opts.inject_recent_user_messages or false
    )
    smelt.session.messages(replacement)
    local after_tokens = approx_history_tokens(replacement)
    emit_event("summarize", before_tokens, after_tokens)
  end)
end

-- ── /compact command ──────────────────────────────────────────────────

smelt.cmd.register("compact", function(arg)
  consecutive_failures = 0
  run_compact({ instructions = arg, inject_recent_user_messages = false })
end, { desc = "compact conversation history", while_busy = false })

-- ── post-turn auto-compaction ─────────────────────────────────────────
--
-- Subscribes to `turn_complete`. Fires when settings.auto_compact is on AND
-- the live prompt token count crosses `compact_threshold` of the configured
-- context window. Uses BeforeLastUserMessage so the user's most recent asks
-- carry forward into the compacted history. A circuit breaker disables
-- auto-firing after `MAX_CONSECUTIVE_FAILURES` consecutive failed attempts
-- so a broken summariser model can't drain the user's tokens in a loop.

smelt.cell("turn_complete"):subscribe(function()
  if not smelt.settings.auto_compact then return end
  if consecutive_failures >= MAX_CONSECUTIVE_FAILURES then return end
  local window = smelt.session.context_window()
  local tokens = smelt.session.context_tokens()
  if not window or not tokens then return end
  local threshold = smelt.settings.compact_threshold
  if not (threshold > 0 and threshold <= 1) then threshold = 0.80 end
  if tokens < window * threshold then return end
  run_compact({ inject_recent_user_messages = true })
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
  local before_tokens = approx_history_tokens(messages)

  -- Cascading fallback: try phase A first (cheap, no LLM). If pruning
  -- alone reclaims enough space the recovery returns the pruned list
  -- without a summarisation round-trip.
  local pruned, elided_bytes = prune_old_tool_results(messages)
  if elided_bytes > 0 then
    emit_event("prune-recovery", before_tokens, approx_history_tokens(pruned),
      { elided_bytes = elided_bytes })
    -- Try with just the prune. If the engine still 413s, this hook will
    -- fire again and we'll fall through to summarisation.
    reply(pruned)
    return
  end

  -- Mid-turn recovery uses the flat path: the engine hands us a `messages`
  -- snapshot that doesn't match `smelt.session.messages()`, so we can't
  -- inherit_session cleanly. Cache reuse is worth less here anyway — the
  -- turn is already failing.
  summarize_flat(messages, nil, function(summary)
    if not summary then
      consecutive_failures = consecutive_failures + 1
      reply(nil)
      return
    end
    consecutive_failures = 0
    local replacement = { { role = "user", content = SUMMARY_PREFIX .. "\n" .. summary } }
    emit_event("summarize-recovery", before_tokens, approx_history_tokens(replacement))
    reply(replacement)
  end)
end)
