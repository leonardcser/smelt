-- Default main TUI layout composer, top to bottom:
--   * `smelt.headerline`  (its rows while a header source shows items)
--   * the transcript       (fill)
--   * `smelt.prompt.node` or the active dialog
--   * `smelt.statusline`   (its rows, at least 1)
--
-- A blocking dialog replaces the whole prompt block so queued input, stash
-- state, tips and notifications cannot compete with the decision. Their state
-- remains intact and returns after the dialog closes.
--
-- Plugins that want a different shape call `smelt.ui.layout.set(fn)` with
-- their own composer; passing `nil` (or never calling `set`) means the engine
-- falls back to its hardcoded seed layout, not this one. Loading this module
-- installs the default composer and returns it as `compose`, so a plugin can
-- wrap the default tree instead of rebuilding it:
--
--   local default = require("smelt.layout").compose
--   smelt.ui.layout.set(function(state)
--     return smelt.ui.layout.hbox({ { default(state), width = "fill" }, ... })
--   end)

-- Minimum rows kept for the transcript even when the prompt block wants
-- to grow.
local MIN_TRANSCRIPT_ROWS = 2

local M = {}

function M.compose(state)
  local layout = smelt.ui.layout
  local header, header_rows = smelt.headerline:node()
  local status, status_rows = smelt.statusline:node()

  local content = state.dialog
  if not content then
    local prompt, prompt_rows = smelt.prompt.node({
      input_rows = state.prompt_input_rows or 1,
      max_rows = (state.term_h or 24) - header_rows - status_rows - MIN_TRANSCRIPT_ROWS,
    })
    content = layout.vbox({
      { height = "fill", layout.leaf(smelt.transcript.win) },
      { height = prompt_rows, prompt },
    })
  end

  return layout.vbox({
    { height = header_rows, header },
    { height = "fill", content },
    { height = status_rows, status },
  })
end

smelt.ui.layout.set(M.compose)

return M
