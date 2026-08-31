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
      _entry         = entry,
      _history_index = i,
      _hay           = label .. " " .. entry,
    }
  end
  return items
end

local function rank_history(items, query, original)
  local by_history_index = {}
  for pos, item in ipairs(items) do
    local source = original[item._idx]
    if source and source._history_index then
      by_history_index[source._history_index] = pos
    end
  end

  local out = {}
  for _, match in ipairs(smelt.history.search(query or "")) do
    local pos = by_history_index[match.index]
    if pos then out[#out + 1] = pos end
  end
  return out
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
      return smelt.picker.open({
        items = build_items(),
        placement = "prompt_docked",
        rank = rank_history,
      })
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
