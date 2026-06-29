-- Terminal notification helpers and transient turn-notification state.

---@class smelt.notifications.Status
---@field configured boolean Persistent turn-end notifications from `smelt.settings.notifications.turn_end`.
---@field once boolean One-shot notification for the next successful turn end.
---@field session boolean True when turn notifications are enabled for this app session.
---@field suppressed boolean True when turn notifications are disabled for this app session.
---@field override "on"|"off"|nil Session override, or nil when following config.
---@field enabled boolean True when the next successful turn end will notify.
---@field mode string Human-readable effective mode.

local M = {}

local once = false
local session_override = nil

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local function configured()
  local ok, value = pcall(function() return smelt.settings.notifications end)
  return ok and type(value) == "table" and value.turn_end == true
end

local function terminal_name(info)
  local values = {}
  for _, value in ipairs({ info.term_program, info.term, info.color_term }) do
    if value and value ~= "" then values[#values + 1] = value end
  end
  return table.concat(values, " "):lower()
end

local function supports_osc9(info)
  local name = terminal_name(info)
  return name:find("ghostty", 1, true) ~= nil
    or name:find("iterm", 1, true) ~= nil
    or name:find("xterm-kitty", 1, true) ~= nil
    or name:find("kitty", 1, true) ~= nil
    or name:find("warp", 1, true) ~= nil
    or name:find("wezterm", 1, true) ~= nil
end

local function effective_enabled(is_configured)
  if once then return true end
  if session_override ~= nil then return session_override == true end
  return is_configured
end

local function mode_label(status)
  if status.once then return "next turn" end
  if status.session then return "session" end
  if status.suppressed then return "off for session" end
  if status.configured then return "config" end
  return "off"
end

--- Send a terminal notification using the best supported terminal primitive.
---@type fun(message: string): boolean
function M.send(message)
  message = trim(message)
  if message == "" then return false end

  local info = smelt.terminal.info()
  if supports_osc9(info) then
    return smelt.terminal.osc9_notify(message, { dcs_passthrough = info.tmux == true })
  end
  return smelt.terminal.bell()
end

--- Notify at the next successful turn end, then clear the one-shot flag.
---@type fun()
function M.enable_once()
  once = true
end

--- Notify at every successful turn end until cleared or smelt exits.
---@type fun()
function M.enable_session()
  session_override = true
end

--- Suppress turn-end notifications until cleared or smelt exits.
---@type fun()
function M.disable_session()
  session_override = false
end

--- Clear the session override and follow `smelt.settings.notifications.turn_end` again.
---@type fun()
function M.clear_session()
  session_override = nil
end

--- Clear one-shot and session override state.
---@type fun()
function M.clear()
  once = false
  session_override = nil
end

--- Return the current persistent and transient turn-end notification state.
---@type fun(): smelt.notifications.Status
function M.status()
  local is_configured = configured()
  local status = {
    configured = is_configured,
    once = once,
    session = session_override == true,
    suppressed = session_override == false,
    override = session_override == nil and nil or (session_override and "on" or "off"),
    enabled = effective_enabled(is_configured),
  }
  status.mode = mode_label(status)
  return status
end

--- Return true when a turn_end payload should produce a notification, consuming one-shot state atomically.
---@type fun(payload: table?): boolean
function M.consume_turn_end(payload)
  payload = payload or {}
  if payload.cancelled or payload.retry_at_ms then return false end
  if once then
    once = false
    return true
  end
  if session_override ~= nil then return session_override == true end
  return configured()
end

smelt.notifications = M

return M
