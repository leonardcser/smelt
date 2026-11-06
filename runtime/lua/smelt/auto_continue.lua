-- Idle auto-continue policy, scheduling, and generic continuation requests.

local M = {}

local AUTO_DELAY_MS = 1200
local QUOTA_RETRY_INITIAL_MS = 60000
local QUOTA_RETRY_MAX_MS = 300000
local RESET_MARGIN_MS = 1000
local USAGE_POLL_MS = 300000

local pending
local retry_state = smelt.state.get("auto_continue")
local timer
local poll_timer
local subscriptions = {}
local setup_done = false
local provider
local resume_policy
local usage_provider = require("smelt.usage_provider")

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local function mode()
  if not smelt.settings then return "goal" end
  local value = smelt.settings.auto_continue
  if value == "off" or value == "always" then return value end
  return "goal"
end

local function prompt_is_empty()
  if not (smelt.prompt and smelt.prompt.text) then return true end
  local ok, text = pcall(smelt.prompt.text)
  return not ok or trim(text or "") == ""
end

local function global_continuation_prompt()
  return table.concat({
    "# Continue",
    "",
    "Continue from the previous turn only if it left concrete work unfinished.",
    "",
    "## Instructions",
    "- Continue only when the prior turn clearly identified actionable remaining work in the user's existing scope.",
    "- If there is no clear unfinished work, say that no continuation is needed and stop.",
    "- Do not invent unrelated work or broaden the user's scope.",
    "- Otherwise, pick the next concrete step and execute it until this turn reaches a useful stopping point.",
  }, "\n")
end

local function generic_request()
  return {
    name = "continue",
    body = global_continuation_prompt(),
    display = "continue",
  }
end

local function auto_continue_request()
  local current_mode = mode()
  if current_mode == "off" then return nil end
  if provider then
    local request = provider(current_mode)
    if request then return request end
  end
  if current_mode == "always" then return generic_request() end
  return nil
end

local function now_ms()
  return smelt.time.now_ms()
end

local function recoverable_pause(state)
  return state.paused and state.token
    and (state.error_kind == "quota" or state.error_kind == "rate_limited")
end

-- Resuming a paused turn finishes work the user already asked for, so only
-- `off` or the resume policy stops it. New work follows the continuation policy.
local function can_continue(state)
  if state.paused then
    return recoverable_pause(state) and mode() ~= "off"
      and (not resume_policy or resume_policy(state))
  end
  return auto_continue_request() ~= nil
end

local function current(schedule, state)
  return schedule and schedule.session == smelt.session.id() and schedule.token == state.token
end

-- A paused turn resumes when the provider's reset time passes. Without a
-- usable reset time it retries with a doubling delay, and only after a first
-- failed attempt.
local function quota_schedule(state)
  local retry = retry_state.recovery
  if current(retry, state) then return retry end
  if not retry or retry.session ~= smelt.session.id() or not retry.awaiting_result then
    retry_state.recovery = nil
    if not state.retry_at_ms then return nil end
    retry = { session = smelt.session.id(), delay_ms = QUOTA_RETRY_INITIAL_MS }
  end

  local now = now_ms()
  -- The latest reported reset wins, unless a retry already ran past it.
  local reset_at_ms = state.retry_at_ms and state.retry_at_ms + RESET_MARGIN_MS or retry.reset_at_ms
  if reset_at_ms and retry.last_attempt_at_ms and reset_at_ms <= retry.last_attempt_at_ms then
    reset_at_ms = nil
  end
  retry.reset_at_ms = reset_at_ms
  retry.token = state.token
  retry.kind = state.error_kind or "other"
  retry.awaiting_result = false
  retry.at_ms = math.max(now + AUTO_DELAY_MS, reset_at_ms or now + retry.delay_ms)
  retry_state.recovery = retry
  return retry
end

local function stop_timer()
  if timer then timer:remove(); timer = nil end
end

local function stop_poll()
  if poll_timer then poll_timer:remove(); poll_timer = nil end
end

local function publish_status()
  local state = smelt.engine.continuation_state()
  local scheduled_token
  if not state.paused and current(pending, state) and can_continue(state) then
    scheduled_token = pending.token
  end
  __smelt_internal.engine.__set_scheduled_continuation(smelt.session.id(), scheduled_token)
  local status
  if state.paused and state.error_kind ~= "cancelled" then
    status = { kind = state.error_kind or "other", phase = "paused" }
    if current(pending, state) and can_continue(state) then
      status.next_attempt_at_ms = pending.at_ms
      status.phase = now_ms() >= pending.at_ms and "waiting_for_idle" or "scheduled"
    end
  else
    -- A resumed turn keeps the pause line until it produces output.
    local retry = retry_state.recovery
    if retry and retry.session == smelt.session.id() and retry.awaiting_result
      and not retry.output_seen and not state.paused then
      status = { kind = retry.kind, phase = "resuming" }
    end
  end
  local previous = smelt.signal.get("auto_continue_status")
  if (status and not previous) or (previous and not status)
    or (status and previous and (status.kind ~= previous.kind or status.phase ~= previous.phase
      or status.next_attempt_at_ms ~= previous.next_attempt_at_ms)) then
    smelt.signal.set("auto_continue_status", status)
  end
end

local function note_output()
  local retry = retry_state.recovery
  if retry and retry.awaiting_result and not retry.output_seen then
    retry.output_seen = true
    publish_status()
  end
end

local function clear_pending()
  stop_timer()
  stop_poll()
  pending = nil
end

function M.continue(continuation_token)
  local state = smelt.engine.continuation_state()
  if state.paused then
    if not can_continue(state) then return false end
    local retry = retry_state.recovery
    local tracked = continuation_token ~= nil and current(retry, state)
    -- Marked before the resume so the status never falls back to idle.
    if tracked then retry.awaiting_result, retry.output_seen = true, nil end
    local started = continuation_token ~= nil and smelt.engine.resume_paused(continuation_token)
    if tracked then
      if started then
        retry.last_attempt_at_ms = now_ms()
        -- The delay grows only over attempts that were not made at a reset time.
        if retry.reset_at_ms then
          if retry.reset_at_ms <= retry.last_attempt_at_ms then retry.reset_at_ms = nil end
        else
          retry.delay_ms = math.min(retry.delay_ms * 2, QUOTA_RETRY_MAX_MS)
        end
      else
        retry.awaiting_result = false
      end
    end
    return started
  end
  local request = auto_continue_request()
  if not request then return false end
  if continuation_token then
    return smelt.engine.submit_command_continuation(
      request.name, request.body, nil, request.display, continuation_token
    ) ~= false
  end
  smelt.engine.submit_command(request.name, request.body, nil, request.display)
  return true
end

local function arm(delay)
  stop_timer()
  local scheduled = pending
  timer = smelt.timer.set(math.max(1, math.floor(delay)), function()
    if pending ~= scheduled then return end
    timer = nil
    local state = smelt.engine.continuation_state()
    if not current(scheduled, state) or not can_continue(state) then
      clear_pending()
    elseif smelt.engine.has_active_turn() or smelt.work.is_busy() or smelt.prompt.is_modal()
      or not prompt_is_empty() or (not state.paused and #smelt.prompt.queued() > 0) then
      -- Keep the same continuation while a process, dialog, or draft temporarily owns idle time.
      arm(250)
    elseif now_ms() < scheduled.at_ms then
      arm(scheduled.at_ms - now_ms())
    elseif M.continue(scheduled.token) then
      clear_pending()
    elseif current(scheduled, smelt.engine.continuation_state()) then
      arm(250)
    else
      clear_pending()
    end
    publish_status()
  end)
end

-- Whether a usage report shows room under every limit it reports.
local function has_room(report)
  local rows = { report.summary }
  for _, row in ipairs(report.limits or {}) do rows[#rows + 1] = row end
  local known = false
  for _, row in ipairs(rows) do
    local limit = tonumber(row.limit) or 0
    if limit > 0 then
      if (tonumber(row.used) or 0) >= limit then return false end
      known = true
    end
  end
  return known
end

-- A reset can come before the time the provider reported. Where the provider
-- reports account usage, a paused turn checks it now and then and resumes as
-- soon as the limits have room.
local function check_usage(scheduled)
  local kind = usage_provider.detect()
  if not kind then return end
  smelt.spawn(function()
    local report, err = smelt.auth.managed_usage(kind)
    if type(report) ~= "table" then
      smelt.log.warn("auto_continue.usage_check_failed", { error = tostring(err) })
      return
    end
    if pending ~= scheduled or scheduled.usage_resumed or not has_room(report) then return end
    -- A report that shows room but is followed by another limit error is not trusted again.
    scheduled.usage_resumed = true
    scheduled.at_ms = now_ms()
    arm(0)
    publish_status()
  end)
end

local function arm_poll(scheduled)
  stop_poll()
  poll_timer = smelt.timer.set(USAGE_POLL_MS, function()
    poll_timer = nil
    if pending ~= scheduled or scheduled.usage_resumed
      or scheduled.at_ms - now_ms() <= USAGE_POLL_MS then
      return
    end
    check_usage(scheduled)
    arm_poll(scheduled)
  end)
end

function M.refresh()
  local state = smelt.engine.continuation_state()
  local retry = retry_state.recovery
  local recovery
  if recoverable_pause(state) then
    recovery = quota_schedule(state)
  elseif not (retry and retry.session == smelt.session.id()
    and retry.awaiting_result and smelt.engine.has_active_turn()) then
    retry_state.recovery = nil
  end
  if not state.token or not can_continue(state) or (state.paused and not recovery) then
    clear_pending()
  else
    if not current(pending, state) then
      clear_pending()
      pending = recovery or {
        token = state.token,
        session = smelt.session.id(),
        at_ms = now_ms() + AUTO_DELAY_MS,
      }
    end
    if not timer then arm(pending.at_ms - now_ms()) end
    if state.paused and not poll_timer and not pending.usage_resumed
      and pending.at_ms - now_ms() > USAGE_POLL_MS then
      arm_poll(pending)
    end
  end
  publish_status()
end

function M.set_provider(fn)
  provider = fn
end

-- `fn(state)` says whether the turn `smelt.engine.continuation_state()` reports
-- as paused by a quota or rate limit may resume.
function M.set_resume_policy(fn)
  resume_policy = fn
end

function M.schedule(continuation_token)
  local state = smelt.engine.continuation_state()
  if continuation_token and continuation_token ~= state.token then return end
  if state.token or state.paused then
    M.refresh()
  elseif auto_continue_request() then
    clear_pending()
    pending = { session = smelt.session.id(), at_ms = now_ms() + AUTO_DELAY_MS }
    arm(AUTO_DELAY_MS)
  end
end

function M.schedule_quota(ev)
  if ev and ev.continuation_token == smelt.engine.continuation_state().token then
    M.refresh()
  end
end

function M.setup()
  if setup_done then return end
  setup_done = true
  for _, name in ipairs({ "stream_delta", "tool_start" }) do
    subscriptions[#subscriptions + 1] = smelt.events.on(name, note_output)
  end
  subscriptions[#subscriptions + 1] = smelt.events.on("turn_end", function(ev)
    if ev.error_kind ~= "quota" and ev.error_kind ~= "rate_limited" then
      retry_state.recovery = nil
    end
    M.refresh()
  end)
  for _, name in ipairs({ "work_continuation_token", "work_pause_kind", "settings_auto_continue", "session_epoch" }) do
    subscriptions[#subscriptions + 1] = smelt.signal.subscribe(name, M.refresh)
  end
  subscriptions[#subscriptions + 1] = smelt.lifecycle.on_ready(M.refresh)
end

return M
