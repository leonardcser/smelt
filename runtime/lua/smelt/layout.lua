-- Default main TUI layout composer.
--
-- Splits the screen vertically into:
--   * transcript           (fill)
--   * prompt block         (the four chrome+input rows, contiguous)
--       - top bar          (rows = queued + stash + 1 bar row)
--       - prompt input     (rows = state.prompt_input_rows)
--       - bottom bar       (1 row)
--       - statusline       (1 row)
--
-- A single gap row sits between the transcript and the prompt block,
-- matching the byte-for-byte output of the previous Rust composer. The
-- inner block has no gap, so its leaves are contiguous.
--
-- Plugins that want a different shape call `smelt.ui.layout.set(fn)`
-- with their own composer; passing `nil` (or never calling `set`) means
-- the engine falls back to its hardcoded seed layout, not this one.
-- Loading this module installs the default composer.

local prompt_bar = require("smelt.prompt_bar")
local statusline = require("smelt.statusline")

smelt.ui.layout.set(function(state)
  local top_rows = prompt_bar.top_rows()
  local input_rows = state.prompt_input_rows or 1
  local block_height = top_rows + input_rows + 1 + 1
  return smelt.ui.layout.vbox({
    {
      height = "fill",
      smelt.ui.layout.leaf(smelt.win.TRANSCRIPT),
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
  }, { gap = 1 })
end)

return {}
