-- Ctrl+R reverse history search. Filterable picker over past prompts;
-- Enter commits to prompt, Esc restores previous text.

-- Rows requested per keystroke. Ranking runs over the whole history in Rust;
-- only the best matches cross into Lua and the picker.
local RESULT_LIMIT = 500

local function entry_label(entry)
  for line in (entry or ""):gmatch("[^\r\n]+") do
    local trimmed = line:match("^%s*(.-)%s*$")
    if trimmed ~= "" then return trimmed end
  end
  return ""
end

-- Items in history order, so a 1-based history index is also an item index.
local function build_items(entries)
  local items = {}
  for i, entry in ipairs(entries) do
    items[i] = { label = entry_label(entry), _entry = entry }
  end
  return items
end

local function rank_history(_, query)
  return smelt.history.search(query or "", RESULT_LIMIT)
end

local saved_text
local is_open = false

local function open()
  if is_open then return end
  local entries = smelt.history.entries()
  if #entries == 0 then return end
  is_open = true
  saved_text = smelt.prompt.text()
  smelt.spawn(function()
    local ok, r = pcall(function()
      return smelt.picker.open({
        items = build_items(entries),
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

smelt.keymap.set("", "c-r", function()
  if smelt.vim.mode() == "normal" then return false end
  open()
end)

smelt.cmd.register("history", open, { desc = "search prompt history" })
