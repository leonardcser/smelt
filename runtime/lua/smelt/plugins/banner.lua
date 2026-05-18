-- Empty-state logo overlay + shutdown logo/resume-hint banner.
--
-- The splash is a non-focusable overlay centered over the transcript on
-- zero-message sessions; it tears down on the first turn. On clean shutdown
-- the same logo + dimmed version + resume hint print to the scrollback.
--
-- The version label is a real buffer so users can select / copy it. Art
-- lives in `smelt.banner` — override `FIRE_PIXELS` / `WORDMARK_PIXELS` /
-- `PALETTE` to retheme, or disable this module via
-- `smelt.builtins.disable({ plugins = { "banner" } })` in `early.lua`.

local banner = require("smelt.banner")

local state = { overlay = nil, paint = nil, version_buf = nil, version_win = nil }

local function teardown()
  if state.overlay then state.overlay:close() end
  if state.paint then state.paint:remove() end
  state.overlay = nil
  state.paint = nil
  state.version_buf = nil
  state.version_win = nil
end

local function paint_logo(slice, _ctx)
  local w = banner.logo_mark_size()
  local col0 = math.max(0, math.floor((slice:width() - w) / 2))
  banner.paint_pixels(slice, 0, col0, banner.LOGO_MARK_PIXELS)
end

local function ensure_version_window(text)
  local buf = smelt.buf.new({ name = "smelt.banner.version.buf" })
  buf:lines({ text })
  local ns = smelt.ns("smelt.banner.version")
  buf:clear_ns(ns)
  buf:mark(ns, 1, 0, { end_col = #text, dim = true })
  local win = smelt.win.new(buf, {
    name = "smelt.banner.version.win",
    focusable = false,
    selectable = true,
  })
  state.version_buf = buf
  state.version_win = win
  return win
end

local function open_splash()
  if state.overlay then return end
  state.paint = smelt.paint.register(paint_logo, { name = "smelt.banner.splash.paint" })
  local logo_w, logo_h = banner.logo_mark_size()
  local version_text = "v" .. (smelt.version or "")
  local w = math.max(logo_w, #version_text)
  local version_win = ensure_version_window(version_text)
  -- Paint slot on top, version buffer below. `measure` pins each slot's
  -- natural width to `w` so the overlay centers exactly.
  local sized = smelt.overlay.layout.vbox({
    {
      smelt.overlay.layout.leaf(state.paint, { measure = { w, logo_h } }),
      height = logo_h,
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
    -- The transcript's bottom gap row pulls integer-center math half a
    -- row above true center on odd heights; nudge down by 1.
    row_offset = 1,
    -- Sits behind dialogs and plugin overlays (default z = 50).
    z = 0,
    modal = false,
    blocks_agent = false,
    border = "none",
    layout = sized,
  })
  -- Center the version text inside the bottom slot via leading padding.
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

-- session_started covers /reset, /fork, /resume; turn_start covers the
-- first dispatch; history covers rewind / compaction / load. on_ready
-- ensures the host pointer is live before the first paint.
smelt.cell("session_started"):subscribe(refresh)
smelt.cell("turn_start"):subscribe(teardown)
smelt.cell("history"):subscribe(refresh)
smelt.lifecycle.on_ready(refresh)

smelt.lifecycle.on_shutdown(function(ctx)
  if not ctx.has_messages then return end
  local rows = banner.LOGO_MARK_PIXELS
  local version_text = "v" .. (smelt.version or "")
  local pad = math.max(0, math.floor((#rows[1] - #version_text) / 2))
  print(banner.ansi_render(rows, banner.PALETTE))
  print(string.rep(" ", pad) .. "\27[2m" .. version_text .. "\27[0m")
  print("")
  io.write(string.format("\27[2mresume with:\nsmelt --resume %s\27[0m\n\n", ctx.session_id))
end)
