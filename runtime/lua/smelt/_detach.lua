-- The action that stops waiting for a foreground command or agent while it
-- keeps running: its keymap description and the label of the key bound to it.

local M = {}

--- Keymap description of the action; `smelt.plugins.process_control` binds it.
M.DESC = "move running command or agent to background"

local MODIFIER_NAMES = { C = "ctrl", M = "alt", S = "shift" }

--- Display form of the key bound to the action (`<C-g>` -> `ctrl+g`), or nil
--- when no key is bound to it.
---@return string?
function M.key_label()
  for _, row in ipairs(smelt.keymap.list()) do
    if row.desc == M.DESC then
      local inner = row.chord:match("^<(.+)>$")
      if not inner then return row.chord end
      local parts = {}
      for part in inner:gmatch("[^-]+") do
        parts[#parts + 1] = MODIFIER_NAMES[part] or part:lower()
      end
      return table.concat(parts, "+")
    end
  end
  return nil
end

return M
