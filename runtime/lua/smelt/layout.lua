-- Default main TUI layout composer, top to bottom:
--   * `smelt.headerline`  (its rows while a header source shows items)
--   * the transcript       (fill)
--   * `smelt.transcript.pending` (its rows while blocks wait to join the transcript)
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

-- Rows between the transcript and the pending blocks under it: the gap the
-- transcript keeps between blocks.
local PENDING_GAP_ROWS = 1

local M = {}

function M.compose(state)
  local layout = smelt.ui.layout
  local header, header_rows = smelt.headerline:node()
  local status, status_rows = smelt.statusline:node()

  local content = state.dialog
  if not content then
    -- Pending blocks sit one gap row below the transcript, where they land.
    local transcript = layout.leaf(smelt.transcript.win)
    local pending, pending_rows = smelt.transcript.pending:node()
    if pending_rows > 0 then
      transcript = layout.vbox({
        { height = "fill", transcript },
        { height = pending_rows, pending },
      }, { gap = PENDING_GAP_ROWS })
      pending_rows = pending_rows + PENDING_GAP_ROWS
    end
    local prompt, prompt_rows = smelt.prompt.node({
      input_rows = state.prompt_input_rows or 1,
      max_rows = (state.term_h or 24) - header_rows - status_rows - pending_rows - MIN_TRANSCRIPT_ROWS,
    })
    content = layout.vbox({
      { height = "fill", transcript },
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
