-- Default main TUI layout composer.
--
-- Splits the screen vertically into:
--   * headerline           (1 row when a header source is visible)
--   * transcript           (fill)
--   * composer or dialog
--       - normal composer: auxiliary row, queued/stash top bar, prompt, bottom bar
--       - active dialog: root-docked modal content replacing the whole composer
--   * statusline           (1 row, always visible)
--
-- A blocking dialog removes the auxiliary row and every composer-chrome row so
-- queued input, stash state, tips, and notifications cannot compete with the
-- decision. Their state remains intact and returns after the dialog closes.
--
-- Plugins that want a different shape call `smelt.ui.layout.set(fn)`
-- with their own composer; passing `nil` (or never calling `set`) means
-- the engine falls back to its hardcoded seed layout, not this one.
-- Loading this module installs the default composer.

local prompt_bar = require("smelt.prompt_bar")
local statusline = require("smelt.statusline")
local headerline = require("smelt.headerline")

-- Minimum rows kept for the transcript even when the prompt block wants
-- to grow.
local MIN_TRANSCRIPT_ROWS = 2

smelt.ui.layout.set(function(state)
  local term_h = state.term_h or 24
  local input_rows = state.prompt_input_rows or 1
  local header_rows = headerline.rows()

  local content
  if state.dialog then
    content = smelt.ui.layout.vbox({
      {
        height = state.dialog_expanded and (state.dialog_transcript_rows or 5) or "fill",
        smelt.ui.layout.leaf(smelt.win.TRANSCRIPT),
      },
      {
        height = state.dialog_height or "fit",
        state.dialog,
      },
    })
  else
    local aux_rows = 1
    -- Reserve the prompt bottom bar and the root statusline while deciding how
    -- many queued/stashed rows the top bar may claim.
    local chrome_except_top = input_rows + 2
    local max_top_rows = math.max(
      1,
      term_h - header_rows - MIN_TRANSCRIPT_ROWS - aux_rows - chrome_except_top)
    local top_rows = prompt_bar.top_rows(max_top_rows)
    local composer_height = top_rows + input_rows + 1

    content = smelt.ui.layout.vbox({
      {
        height = "fill",
        smelt.ui.layout.leaf(smelt.win.TRANSCRIPT),
      },
      {
        height = aux_rows,
        smelt.ui.layout.leaf(prompt_bar.aux_win),
      },
      {
        height = composer_height,
        smelt.ui.layout.vbox({
          {
            height = top_rows,
            smelt.ui.layout.leaf(prompt_bar.top_win),
          },
          {
            height = input_rows,
            smelt.ui.layout.leaf(smelt.win.PROMPT),
          },
          {
            height = 1,
            smelt.ui.layout.leaf(prompt_bar.bottom_win),
          },
        }),
      },
    })
  end

  local main = smelt.ui.layout.vbox({
    { height = "fill", content },
    { height = 1, smelt.ui.layout.leaf(statusline.win) },
  })

  if header_rows == 0 then return main end
  return smelt.ui.layout.vbox({
    {
      height = header_rows,
      smelt.ui.layout.leaf(headerline.win),
    },
    {
      height = "fill",
      main,
    },
  })
end)

return {}
