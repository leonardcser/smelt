-- Compaction plugin. Owns the /compact command and the post-turn
-- auto-compact subscription. Replaces the conversation with a model-generated
-- handoff summary so future turns fit within the context window.
--
-- Drives the status-bar spinner via `smelt.spinner.busy("compacting")`
-- and uses `smelt.engine.ask_with_trim` so the summarizer request itself
-- can drop the oldest history entry and retry when it overflows.

local SYSTEM = [[
You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another instance of yourself that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.
]]

local SUMMARY_PREFIX = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:"

local INSTRUCTIONS_PREAMBLE = "The user has asked you to pay special attention to the following when summarizing:"

-- Per-message byte cap when flattening history for the summarizer.
local MAX_STRINGIFIED_MESSAGE_BYTES = 8000
local TRUNCATION_SUFFIX = "\n…[truncated for compaction]"

-- Soft token cap on user messages carried forward in BeforeLastUserMessage mode.
local COMPACT_USER_MESSAGE_MAX_TOKENS = 20000

-- Fire auto-compact when the prompt usage crosses this fraction of the window.
-- Override per user: `smelt.state.persistent("compact").threshold = 0.7` in init.lua.
local DEFAULT_AUTO_COMPACT_THRESHOLD = 0.80

local function auto_compact_threshold()
  local v = smelt.state.persistent("compact").threshold
  if type(v) == "number" and v > 0 and v <= 1 then return v end
  return DEFAULT_AUTO_COMPACT_THRESHOLD
end

-- How many times to re-issue the summarizer call when the model returns an
-- empty response before giving up.
local MAX_EMPTY_RETRIES = 2

-- ── helpers ────────────────────────────────────────────────────────────

local function approx_tokens(s) return math.ceil(#s / 4) end

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
-- as `user` content. Mirrors the previous Rust `stringify_conversation`:
-- labels by role, trims, drops blanks, caps each entry's bytes.
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

-- `{ system, user }` message pair the summarizer model receives.
local function compact_request_messages(history, instructions)
  local system_text = SYSTEM:gsub("^%s+", ""):gsub("%s+$", "")
  if instructions then
    local extra = instructions:gsub("^%s+", ""):gsub("%s+$", "")
    if extra ~= "" then
      system_text = system_text .. "\n\n" .. INSTRUCTIONS_PREAMBLE .. "\n" .. extra
    end
  end
  local user_text = "Conversation to summarize:\n\n" .. stringify_conversation(history)
  return system_text, {
    { role = "user", content = user_text },
  }
end

-- Async summarizer primitive shared by /compact, post-turn auto-compact,
-- and the on_context_limit recovery hook. Owns the spinner, the
-- trim-on-overflow + empty-retry loops, and notification on failure.
-- Calls `done(summary_string)` on success or `done(nil)` on failure.
local function summarize(history, instructions, done)
  if not history or #history == 0 then
    done(nil)
    return
  end
  local system_text, messages = compact_request_messages(history, instructions)
  local handle = smelt.spinner.busy("compacting")
  local empty_retries = 0

  local function send()
    smelt.engine.ask_with_trim({
      system      = system_text,
      messages    = messages,
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

local function run_compact(opts)
  local history = smelt.session.messages()
  if not history or #history == 0 then
    smelt.notify.error("nothing to compact")
    return
  end
  summarize(history, opts and opts.instructions, function(summary)
    if not summary then return end
    local replacement = build_replacement(
      history,
      summary,
      opts and opts.inject_recent_user_messages or false
    )
    smelt.session.messages(replacement)
  end)
end

-- ── /compact command ──────────────────────────────────────────────────

smelt.cmd.register("compact", function(arg)
  run_compact({ instructions = arg, inject_recent_user_messages = false })
end, { desc = "compact conversation history", while_busy = false })

-- ── post-turn auto-compaction ─────────────────────────────────────────
--
-- Subscribes to `turn_complete`. Fires when settings.auto_compact is on AND
-- the live prompt token count crosses AUTO_COMPACT_THRESHOLD of the configured
-- context window. Uses BeforeLastUserMessage so the user's most recent asks
-- carry forward into the compacted history.

smelt.cell("turn_complete"):subscribe(function()
  if not smelt.settings.auto_compact then return end
  local window = smelt.session.context_window()
  local tokens = smelt.session.context_tokens()
  if not window or not tokens then return end
  if tokens < window * auto_compact_threshold() then return end
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
  summarize(messages, nil, function(summary)
    if not summary then
      reply(nil)
      return
    end
    reply({ { role = "user", content = SUMMARY_PREFIX .. "\n" .. summary } })
  end)
end)
