-- Idle auto-continue policy, scheduling, and generic continuation requests.

local M = {}

local AUTO_DELAY_MS = 1200

local short_schedule_id = 0
local quota_schedule_id = 0
local generation = 0
local setup_done = false
local provider

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

local function should_auto_continue()
  return auto_continue_request() ~= nil
    and not smelt.engine.has_active_turn()
    and not smelt.work.is_busy()
    and prompt_is_empty()
end

local function recoverable_quota_error(ev)
  if not ev or not ev.cancelled then return false end
  if ev.error_kind ~= "quota" and ev.error_kind ~= "rate_limited" then return false end
  return type(ev.retry_at_ms) == "number" and type(ev.continuation_token) == "number"
end

local function now_ms()
  if smelt.time and smelt.time.now_ms then
    local ok, value = pcall(smelt.time.now_ms)
    if ok and type(value) == "number" then return value end
  end
  return os.time() * 1000
end

local function submit(request, continuation_token)
  if continuation_token then
    return smelt.engine.submit_command_continuation(
      request.name,
      request.body,
      nil,
      request.display,
      continuation_token
    ) ~= false
  end
  smelt.engine.submit_command(request.name, request.body, nil, request.display)
  return true
end

function M.set_provider(fn)
  provider = fn
end

function M.bump_generation()
  generation = generation + 1
end

function M.continue(continuation_token)
  local request = auto_continue_request()
  if not request then return false end
  return submit(request, continuation_token)
end

function M.schedule(continuation_token)
  if not auto_continue_request() then return end
  short_schedule_id = short_schedule_id + 1
  local schedule_id = short_schedule_id
  local gen = generation
  smelt.timer.set(AUTO_DELAY_MS, function()
    if schedule_id ~= short_schedule_id or gen ~= generation or not should_auto_continue() then return end
    M.continue(continuation_token)
  end)
end

function M.schedule_quota(ev)
  if not recoverable_quota_error(ev) or not auto_continue_request() then return end
  quota_schedule_id = quota_schedule_id + 1
  local schedule_id = quota_schedule_id
  local gen = generation
  local delay = math.max(AUTO_DELAY_MS, math.floor(ev.retry_at_ms - now_ms()) + 1000)
  smelt.timer.set(delay, function()
    if schedule_id ~= quota_schedule_id or gen ~= generation or not should_auto_continue() then return end
    M.continue(ev.continuation_token)
  end)
end

function M.setup()
  if setup_done then return end
  setup_done = true
  smelt.events.on("turn_end", function(ev)
    if ev and ev.cancelled then
      M.schedule_quota(ev)
      return
    end
    M.schedule(ev and ev.continuation_token)
  end)
end

return M
