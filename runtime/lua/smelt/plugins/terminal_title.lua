-- Keeps the terminal window/tab title in sync with smelt. The displayed title
-- is the session title, then the task slug, then `smelt`. Disable with
-- `smelt.settings.terminal_title = false` or via
-- `smelt.builtins.disable({ plugins = { "terminal_title" } })` in `early.lua`.

local last_title = nil

local function trim(s)
  return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local function terminal_title_enabled()
  local ok, value = pcall(function() return smelt.settings.terminal_title end)
  return ok and value ~= false
end

local function preferred_title()
  local title = trim(smelt.session.title.get())
  if title ~= "" then return title end

  local slug = trim(smelt.session.slug.get())
  if slug ~= "" then return slug end

  slug = trim(smelt.signal.get("task_label"))
  if slug ~= "" then return slug end

  return "smelt"
end

local function refresh()
  if not terminal_title_enabled() then
    if last_title ~= nil then
      smelt.terminal.clear_title()
      last_title = nil
    end
    return
  end

  local title = preferred_title()
  if title == last_title then return end
  smelt.terminal.set_title(title)
  last_title = title
end

refresh()
smelt.signal.subscribe("session_title", refresh)
smelt.signal.subscribe("session_slug", refresh)
smelt.signal.subscribe("task_label", refresh)
smelt.signal.subscribe("settings_terminal_title", refresh)
