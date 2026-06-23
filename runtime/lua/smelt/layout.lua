-- Default main TUI layout composer.
--
-- Splits the screen vertically into:
--   * headerline           (1 row when a header source is visible)
--   * transcript           (fill)
--   * prompt auxiliary row (stable gap: empty, tip, or notification overlay)
--   * prompt block         (chrome+input rows, contiguous)
--       - top bar          (rows = queued + stash + 1 bar row)
--       - prompt input     (rows = state.prompt_input_rows)
--       - bottom bar       (1 row)
--       - statusline       (1 row)
--
-- A stable auxiliary row sits between the transcript and the prompt block,
-- matching the old readability gap while giving tips and notification overlays
-- a consistent home. The inner block has no gap, so its leaves are contiguous.
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

  -- prompt block = top_bar + input_rows + bottom_bar(1) + statusline(1)
  local aux_rows = 1
  local chrome_except_top = input_rows + 2
  local max_top_rows = math.max(
    1,
    term_h - header_rows - MIN_TRANSCRIPT_ROWS - aux_rows - chrome_except_top)
  local top_rows = prompt_bar.top_rows(max_top_rows)
  local block_height = top_rows + chrome_except_top

  local main = smelt.ui.layout.vbox({
    {
      height = "fill",
      smelt.ui.layout.leaf(smelt.win.TRANSCRIPT),
    },
    {
      height = aux_rows,
      smelt.ui.layout.leaf(prompt_bar.aux_win),
    },
    {
      height = block_height,
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
        {
          height = 1,
          smelt.ui.layout.leaf(statusline.win),
        },
      }),
    },
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
