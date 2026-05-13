-- Ctrl+R reverse history search. Filterable picker over past prompts;
-- Enter commits to prompt, Esc restores previous text.

local function entry_label(entry)
  for line in (entry or ""):gmatch("[^\r\n]+") do
    local trimmed = line:match("^%s*(.-)%s*$")
    if trimmed ~= "" then return trimmed end
  end
  return ""
end

local function build_items()
  local entries = smelt.history.entries()
  if #entries == 0 then return {} end
  local items = {}
  for i = #entries, 1, -1 do
    local entry = entries[i]
    local label = entry_label(entry)
    items[#items + 1] = {
      label        = label,
      search_terms = entry,
      _entry       = entry,
      _hay         = label .. " " .. entry,
    }
  end
  return items
end

local saved_text
local is_open = false

local function open()
  if is_open then return end
  is_open = true
  saved_text = smelt.prompt.text()
  if #smelt.history.entries() == 0 then
    is_open = false
    return
  end
  smelt.spawn(function()
    local ok, r = pcall(function()
      return smelt.prompt.open_picker({ items = build_items() })
    end)
    if ok and r and r.action == "enter" then
      smelt.prompt.set_text(r.item._entry or "")
    else
      smelt.prompt.set_text(saved_text or "")
    end
    is_open = false
  end)
end

for _, mode in ipairs({ "normal", "insert", "visual" }) do
  smelt.keymap.set(mode, "c-r", open)
end

smelt.cmd.register("history", open, { desc = "search prompt history" })
