-- Optional terminal desktop notification when an agent turn ends. Disabled by
-- default; enable persistently with `smelt.settings.notifications.turn_end = true`
-- or temporarily with `/notify`.

local notifications = require("smelt.notifications")

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
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

smelt.events.on("turn_end", function(payload)
  if not notifications.consume_turn_end(payload) then return end
  notifications.send(notification_message())
end)
