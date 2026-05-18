-- Empty-state wordmark overlay + shutdown logo/resume-hint banner.
--
-- Anchors a non-focusable overlay centered over the transcript window
-- whenever the session has zero messages, and tears it down the moment a
-- turn begins. On clean shutdown the full logo + wordmark + dimmed
-- version + resume hint print to the cooked terminal scrollback.
--
-- The wordmark is a paint-rendered half-block image; the version label is
-- a real buffer so the user can select / copy it. Art lives in
-- `smelt.banner` — replace `M.LOGO_PIXELS` / `M.WORDMARK_PIXELS` /
-- `M.PALETTE` from a user plugin to retheme. Disable this whole module
-- via `smelt.builtins.disable({ plugins = { "banner" } })` in `early.lua`.

local banner = require("smelt.banner")

local state = { overlay = nil, paint_id = nil, version_buf = nil, version_win = nil }

local function teardown()
  if state.overlay then state.overlay:close() end
  if state.paint_id then smelt.paint.unregister(state.paint_id) end
  state.overlay = nil
  state.paint_id = nil
  state.version_buf = nil
  state.version_win = nil
end

local function paint_wordmark(slice, _ctx)
  local w = banner.wordmark_size()
  local col0 = math.max(0, math.floor((slice:width() - w) / 2))
  banner.paint_pixels(slice, 0, col0, banner.WORDMARK_PIXELS)
end

local function ensure_version_window(text)
  local buf = smelt.buf.new({ name = "smelt.banner.version.buf" })
  buf:lines({ text })
  local ns = smelt.ns("smelt.banner.version")
  buf:clear_ns(ns)
  buf:mark(ns, 1, 0, { end_col = #text, dim = true })
  local win = smelt.win.new(buf, { name = "smelt.banner.version.win", focusable = false })
  state.version_buf = buf
  state.version_win = win
  return win
end

local function open_splash()
  if state.overlay then return end
  state.paint_id = smelt.paint.register(paint_wordmark, { name = "smelt.banner.splash.paint" })
  local word_w, word_h = banner.wordmark_size()
  local version_text = "v" .. (smelt.version or "")
  local w = math.max(word_w, #version_text)
  local version_win = ensure_version_window(version_text)
  -- vbox: paint slot (word_h rows) on top, real-buffer slot (1 row) below.
  -- Per-leaf `measure` hints pin each slot's natural width to `w` so the
  -- overlay's natural rect resolves to exactly `w` cells wide and the
  -- `center` anchor centers that.
  local sized = smelt.overlay.layout.vbox({
    {
      smelt.overlay.layout.leaf(state.paint_id, { measure = { w, word_h } }),
      height = word_h,
    },
    {
      smelt.overlay.layout.leaf(version_win, { measure = { w, 1 } }),
      height = 1,
    },
  })
  state.overlay = smelt.overlay.new({
    name = "smelt.banner.splash",
    anchor = "win",
    target = smelt.win.transcript(),
    attach = "center",
    -- Transcript reserves a row at its bottom for the gap separating it
    -- from the prompt window. That row counts toward the window's height
    -- so geometric-center math lands a half-row low — nudge down by 1 to
    -- restore visual symmetry. (Yes, "down by 1" looks counter-intuitive
    -- with a bottom gap; what's actually happening is the rounded-down
    -- integer center sits one row above true center on an odd-height
    -- viewport.)
    row_offset = 1,
    -- Sits behind dialogs and any other plugin overlay (default z = 50) so
    -- a /resume picker, confirm dialog, or perf panel never has to fight
    -- the splash for the user's attention.
    z = 0,
    modal = false,
    blocks_agent = false,
    border = "none",
    layout = sized,
  })
  -- Re-center the version line inside the bottom slot via a per-row dim
  -- highlight + leading padding. (`buf:lines` writes the raw text; we want
  -- it horizontally centered within `w` cells.)
  local pad = math.floor((w - #version_text) / 2)
  if pad > 0 then
    state.version_buf:lines({ string.rep(" ", pad) .. version_text })
    local ns = smelt.ns("smelt.banner.version")
    state.version_buf:clear_ns(ns)
    state.version_buf:mark(ns, 1, pad, { end_col = pad + #version_text, dim = true })
  end
end

local function refresh()
  local msgs = smelt.session.messages({}) or {}
  if #msgs == 0 then open_splash() else teardown() end
end

-- Subscriptions register inside `on_ready`: it fires once per Lua-context
-- bring-up (cold start AND `/reload`) with the host pointer live, which
-- `smelt.cell:subscribe` needs. The `lifecycle` registry is wiped between
-- bring-ups so re-subscribing here doesn't stack. From this hook onward
-- `session_started` covers /reset, /fork, /resume; `turn_start` covers
-- the first agent dispatch; `history` covers direct message-list
-- mutations (rewind, compaction, load).
smelt.lifecycle.on_ready(function()
  smelt.cell("session_started"):subscribe(refresh)
  smelt.cell("turn_start"):subscribe(teardown)
  smelt.cell("history"):subscribe(refresh)
  refresh()
end)

smelt.lifecycle.on_shutdown(function(ctx)
  if not ctx.has_messages then return end
  local rows, version_col, version_row = banner.compose()
  local version_text = "v" .. (smelt.version or "")
  local overlays = { { row = version_row, col = version_col, text = version_text, dim = true } }
  print(banner.ansi_render(rows, banner.PALETTE, overlays))
  print("")
  io.write(string.format("\27[2mresume with:\nsmelt --resume %s\27[0m\n\n", ctx.session_id))
end)
