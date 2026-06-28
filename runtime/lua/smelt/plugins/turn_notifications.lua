-- Optional terminal desktop notification when an agent turn ends. Disabled by
-- default; enable with `smelt.settings.notifications.turn_end = true`. The backend
-- follows Codex's terminal strategy: OSC 9 on known supporting terminals, BEL as
-- the portable fallback, with tmux DCS passthrough for OSC 9.

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local warned_invalid_method = false

local function warn_invalid_method(value)
  if warned_invalid_method then return end
  warned_invalid_method = true
  if smelt.notify and smelt.notify.warn then
    pcall(smelt.notify.warn, "invalid smelt.settings.notifications.method '" .. tostring(value) .. "'; using auto")
  end
end

local function notifications()
  local ok, value = pcall(function() return smelt.settings.notifications end)
  if not ok or type(value) ~= "table" then
    value = {}
    pcall(function() smelt.settings.notifications = value end)
  end
  if type(value.turn_end) ~= "boolean" then value.turn_end = false end
  if value.method == nil then value.method = "auto" end
  return value
end

local function enabled()
  return notifications().turn_end == true
end

local function method()
  local raw = notifications().method or "auto"
  local value = type(raw) == "string" and raw:lower() or ""
  if value == "osc9" or value == "bel" or value == "auto" then return value end
  warn_invalid_method(raw)
  return "auto"
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

local function preferred_label()
  local title = trim(smelt.session.title.get())
  if title ~= "" then return title end

  local slug = trim(smelt.session.slug.get())
  if slug ~= "" then return slug end

  slug = trim(smelt.signal.get("task_label"))
  if slug ~= "" then return slug end

  return ""
end

local function truncate(s, max)
  if smelt.text and smelt.text.truncate then
    return smelt.text.truncate(s, max, { keep = "head" })
  end
  if #s <= max then return s end
  return s:sub(1, max - 1) .. "…"
end

local function notification_message()
  local label = preferred_label()
  if label == "" then return "smelt turn complete" end
  return truncate("smelt turn complete: " .. label, 160)
end

local function notify(message)
  local info = smelt.terminal.info()
  local selected = method()
  if selected == "auto" then
    selected = supports_osc9(info) and "osc9" or "bel"
  end

  if selected == "osc9" then
    return smelt.terminal.osc9_notify(message, { dcs_passthrough = info.tmux == true })
  end
  return smelt.terminal.bell()
end

smelt.events.on("turn_end", function(payload)
  if not enabled() then return end
  payload = payload or {}
  if payload.cancelled or payload.retry_at_ms then return end
  notify(notification_message())
end)
