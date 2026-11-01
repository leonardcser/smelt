-- Default colorscheme. Mirrors `smelt_tui::theme::baked_default_spec` so
-- the binary's paint-before-bootstrap fallback and the live Lua-applied
-- theme look identical.
--
-- A colorscheme returns a `ThemeSpec` table:
--   {
--     name = "default",
--     syntax = "Monokai Extended", -- optional bundled two-face syntax theme
--     light = nil,                 -- nil means use terminal background detection
--     groups = { ... },
--   }
--
-- Color shape (anywhere a `fg = ...` or `bg = ...` appears):
--   - `N` for an ANSI 256-color slot.
--   - `{ R, G, B }` for a literal sRGB triple.
--   - `{ dark = ..., light = ... }` to branch on terminal background.

local function fg(color, extra)
  local s = { fg = color }
  if extra then for k, v in pairs(extra) do s[k] = v end end
  return s
end

local function bg(color)
  return { bg = color }
end

local function dl(dark, light)
  return { dark = dark, light = light }
end

local accent = 208

local groups = {
  -- Semantic editor groups use nvim-standard names directly.
  Normal              = {},
  Comment             = fg(244), -- comment grey
  Visual              = bg(dl(238, 153)),
  Search              = { fg = 0, bg = accent },
  YankFlash           = bg(dl(240, 117)),
  CursorLine          = bg(dl(237, 251)),
  ErrorMsg            = fg(9),   -- bright red
  WarningMsg          = fg(11),  -- bright yellow
  GhostText           = { dim = true },

  SmeltAccent         = fg(accent),          -- ember
  SmeltSlug           = fg(0),    -- pill fg; bg falls back to SmeltAccent in statusline.lua
  SmeltSuccess        = fg(77),   -- check-mark green
  SmeltHeading        = fg(117),  -- sky blue headings
  SmeltLink           = fg(75),   -- markdown link destinations
  SmeltProcess        = fg(117),  -- background-process notices and counters
  SmeltGoalBanner     = { fg = dl(0, 0), bg = dl(39, 153) }, -- active-goal top banner row
  SmeltGoalBannerLabel = { fg = dl(0, 0), bg = dl(39, 153), bold = true }, -- active-goal label
  SmeltGoalBannerMode = { fg = dl(0, 0), bg = dl(39, 153), bold = true }, -- right-side goal mode
  SmeltGoalBannerPausedLabel = { fg = dl(0, 0), bg = dl(220, 220), bold = true },
  SmeltGoalBannerBlockedLabel = { fg = dl(0, 0), bg = dl(203, 203), bold = true },

  -- Background fills, light/dark aware.
  SmeltStatusBg       = bg(dl(233, 253)),
  SmeltUserBg         = bg(dl(236, 252)),
  SmeltUserTimestamp  = fg({ dark = { 128, 128, 128 }, light = { 112, 112, 112 } }),
  SmeltScrollPillBg   = bg(dl(234, 253)),
  SmeltCodeBlockBg    = bg(dl(233, 253)),
  SmeltSeparator      = fg(dl(237, 250)),
  SmeltResizeHandle   = fg(dl(15, 0), { bold = true }),
  SmeltScrollbarTrack = bg(dl(235, 253)),
  SmeltScrollbarThumb = bg(dl(243, 245)),

  -- Tool and reasoning state colors.
  SmeltToolPending    = fg(dl(8, 244)),
  SmeltReasonLow      = fg(75),
  SmeltReasonMedium   = fg(214),
  SmeltReasonHigh     = fg(203),
  SmeltReasonMax      = fg(196),
  SmeltReasonUltra    = fg(dl(201, 127)),

  -- Statusline pills: each carries a full {fg, bg} pair so plugins
  -- reference them by `style_group` alone.
  SmeltVimNormal      = { fg = 74, bg = dl(236, 254) },
  SmeltVimInsert      = { fg = 78, bg = dl(236, 254) },
  SmeltVimVisual      = { fg = 176, bg = dl(236, 254) },
  SmeltModePlan       = { fg = 79, bg = dl(234, 255) },
  SmeltModeApply      = { fg = 141, bg = dl(234, 255) },
  SmeltModeYolo       = { fg = 204, bg = dl(234, 255) },
  SmeltModeDefault    = { fg = dl(244, 240), bg = dl(234, 255) },

  -- Foreground-only accent for the `!` exec prefix in the prompt and in
  -- the transcript exec block. Bg is intentionally omitted so the prompt
  -- stays unfilled and the transcript block can show `SmeltUserBg`
  -- underneath.
  SmeltExecPrefix     = fg(197, { bold = true }),

  -- Diff renderer row fills and inline change fills. Override these like any other group.
  SmeltDiffAddCount       = fg({ dark = { 113, 192, 117 }, light = { 50, 123, 61 } }),
  SmeltDiffDeleteCount    = fg({ dark = { 219, 104, 104 }, light = { 173, 57, 57 } }),
  SmeltDiffAddBg          = bg({ dark = { 20, 50, 20 }, light = { 218, 242, 218 } }),
  SmeltDiffDeleteBg       = bg({ dark = { 60, 20, 20 }, light = { 248, 218, 218 } }),
  SmeltDiffAddInlineBg    = bg({ dark = { 35, 95, 35 }, light = { 180, 230, 180 } }),
  SmeltDiffDeleteInlineBg = bg({ dark = { 110, 35, 35 }, light = { 242, 175, 175 } }),
}

return {
  name = "default",
  groups = groups,
}
