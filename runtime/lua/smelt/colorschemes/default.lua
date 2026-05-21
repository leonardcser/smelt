-- Default colorscheme. Mirrors `smelt_tui::theme::baked_default_spec` so
-- the binary's paint-before-bootstrap fallback and the live Lua-applied
-- theme look identical.
--
-- A colorscheme `return`s a `ThemeSpec` table: a flat map keyed by
-- highlight-group name (`SmeltAccent`, `Comment`, …) whose values are
-- either a `StyleDecl` table or a string referencing another group.
-- There's no `groups = { ... }` wrapper — top-level keys are groups.
--
-- Color shape (anywhere a `fg = ...` or `bg = ...` appears):
--   - `{ ansi = N }` for an ANSI 256-color slot.
--   - `{ rgb = { R, G, B } }` for a literal sRGB triple.
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
  return { dark = { ansi = dark }, light = { ansi = light } }
end

return {
  -- ── Base palette: groups that hold literal color values. ──────────
  SmeltAccent         = fg({ ansi = 208 }),   -- ember
  SmeltSlug           = fg({ ansi = 0 }),     -- pill fg; bg falls back to SmeltAccent in status.lua
  SmeltMuted          = fg({ ansi = 244 }),   -- "comment grey"
  SmeltSuccess        = fg({ ansi = 77 }),    -- check-mark green
  SmeltHeading        = fg({ ansi = 117 }),   -- sky blue headings

  -- Background fills, light/dark aware.
  SmeltStatusBg       = bg({ ansi = 233 }),   -- always dark; cmdline / status
  SmeltUserBg         = bg(dl(236, 254)),
  SmeltScrollPillBg   = bg(dl(234, 250)),
  SmeltCodeBlockBg    = bg(dl(233, 255)),
  SmeltBar            = fg(dl(237, 252)),
  SmeltSelection      = bg(dl(238, 189)),
  SmeltCursorLineBg   = bg(dl(237, 253)),
  SmeltScrollbarTrack = bg(dl(235, 254)),
  SmeltScrollbarThumb = bg(dl(243, 247)),

  -- Tool / reasoning state colors. `8` is dark grey in the 256-color slot.
  SmeltToolPending    = fg(dl(8, 250)),
  SmeltReasonOff      = fg(dl(8, 250)),
  SmeltReasonLow      = fg({ ansi = 75  }),
  SmeltReasonMed      = fg({ ansi = 214 }),
  SmeltReasonHigh     = fg({ ansi = 203 }),
  SmeltReasonMax      = fg({ ansi = 196 }),

  -- Statusline pills: each carries a full {fg, bg} pair so plugins
  -- reference them by `style_group` alone.
  SmeltCompacting     = { fg = { ansi = 0  }, bg = { ansi = 15  } },
  SmeltVimNormal      = { fg = { ansi = 74 }, bg = { ansi = 236 } },
  SmeltVimInsert      = { fg = { ansi = 78 }, bg = { ansi = 236 } },
  SmeltVimVisual      = { fg = { ansi = 176}, bg = { ansi = 236 } },
  SmeltModePlan       = { fg = { ansi = 79 }, bg = { ansi = 234 } },
  SmeltModeApply      = { fg = { ansi = 141}, bg = { ansi = 234 } },
  SmeltModeYolo       = { fg = { ansi = 204}, bg = { ansi = 234 } },
  SmeltModeExec       = { fg = { ansi = 197}, bg = { ansi = 234 }, bold = true },
  SmeltModeDefault    = { fg = { ansi = 244}, bg = { ansi = 234 } },

  -- Foreground-only accent for the `!` exec prefix in the prompt and in
  -- the transcript exec block. Bg is intentionally omitted so the prompt
  -- stays unfilled and the transcript block can show `SmeltUserBg`
  -- underneath.
  SmeltExecPrefix     = fg({ ansi = 197 }, { bold = true }),

  -- Diff renderer row fills. Override these like any other group.
  SmeltDiffAddBg      = bg({ rgb = { 20, 50, 20 } }),
  SmeltDiffDelBg      = bg({ rgb = { 60, 20, 20 } }),

  -- ── Semantic / nvim-standard names: aliases into the base set. ────
  Comment             = "SmeltMuted",
  Visual              = "SmeltSelection",
  CursorLine          = "SmeltCursorLineBg",
  ErrorMsg            = fg({ ansi = 9 }),   -- bright red
  WarningMsg          = fg({ ansi = 11 }),  -- bright yellow
  GhostText           = { dim = true },
}
