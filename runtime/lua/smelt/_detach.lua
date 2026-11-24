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

-- How long a call waits before its block says how to stop waiting.
local HINT_AFTER_MS = 10000

--- The hint under a call that has waited for 10 seconds or more: the key
--- that leaves its work running in the background. Nil before then, once the
--- call stops waiting, or when no key is bound to the action.
---@param block smelt.transcript.Block
---@return smelt.layout.Node?
function M.waiting_hint(block)
  if not block.elapsed_active or (block.elapsed_ms or 0) < HINT_AFTER_MS then return nil end
  local key = M.key_label()
  if not key then return nil end
  return smelt.layout.line({
    { text = key, hl = "SmeltAccent", selectable = false },
    { text = " to run in background", dim = true, selectable = false },
  })
end

return M
