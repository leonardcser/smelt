-- /banner — interactive picker for the smelt logo art variants.
--
-- Demo of `smelt.overlay` + `smelt.timer` + `smelt.paint` reusing canonical
-- pixel data from `smelt.banner` (LOGO_PIXELS, WORDMARK_PIXELS,
-- MINI_FIRE_FRAMES, PALETTE). Tab cycles variants; Space toggles the
-- animation loop; Esc / Ctrl-C closes.
--
-- Not autoloaded. Add `require("smelt.examples.banner_picker")` to
-- `init.lua` to wire the `/banner` command.
--
-- The overlay is sized to its content via `smelt.overlay.layout.measure(...)`
-- handles: one for the paint surface (updated on Tab to the current variant's
-- pixel size), one static for the hint row. The layout's natural size feeds
-- the overlay's rect every frame, so cycling resizes smoothly with no
-- close+reopen churn.

local banner = require("smelt.banner")

local M = {}

local PERSIST = { variant_idx = 1, looping = false, frame = 0 }
local STATE = nil

local LABELS = {
  "full",
  "full+version",
  "icon",
  "wordmark+version",
  "wordmark+fire",
}

local HINT = " Tab / Space / Esc "
local TICK_MS = 166

local function compose_fire_wordmark(fire, wordmark)
  local fire_w, wm_w = #fire[1], #wordmark[1]
  local width = math.max(fire_w, wm_w)
  local function centered(row, row_w)
    local left = math.floor((width - row_w) / 2)
    return string.rep(".", left) .. row .. string.rep(".", width - left - row_w)
  end
  local rows = {}
  for _, r in ipairs(fire) do rows[#rows + 1] = centered(r, fire_w) end
  for _, r in ipairs(wordmark) do rows[#rows + 1] = centered(r, wm_w) end
  -- Pad at top so the wordmark sits flush against the version line below.
  if #rows % 2 == 1 then table.insert(rows, 1, string.rep(".", width)) end
  return rows
end

-- Returns (cell_width, cell_height, paint_fn(slice, row0, col0)) for the
-- given variant at the given animation frame.
local function variant_info(idx, frame_idx)
  local label = LABELS[idx]
  local version = "v" .. (smelt.version or "")
  if label == "full" then
    local rows = banner.compose(banner.LOGO_PIXELS, banner.WORDMARK_PIXELS)
    return #rows[1], math.ceil(#rows / 2), function(slice, r, c)
      banner.paint_pixels(slice, r, c, rows)
    end
  elseif label == "full+version" then
    local rows, vcol, vrow = banner.compose(banner.LOGO_PIXELS, banner.WORDMARK_PIXELS)
    return #rows[1], math.ceil(#rows / 2), function(slice, r, c)
      banner.paint_pixels(slice, r, c, rows)
      slice:put_str(r + vrow, c + vcol, version, { dim = true })
    end
  elseif label == "icon" then
    local rows = banner.LOGO_PIXELS
    return #rows[1], math.ceil(#rows / 2), function(slice, r, c)
      banner.paint_pixels(slice, r, c, rows)
    end
  elseif label == "wordmark+version" then
    local rows = banner.WORDMARK_PIXELS
    local w, h = #rows[1], math.ceil(#rows / 2)
    return w, h + 1, function(slice, r, c)
      banner.paint_pixels(slice, r, c, rows)
      local pad = math.floor((w - #version) / 2)
      slice:put_str(r + h, c + pad, version, { dim = true })
    end
  else -- "wordmark+fire"
    local frames = banner.MINI_FIRE_FRAMES
    local fire = frames[(frame_idx % #frames) + 1]
    local rows = compose_fire_wordmark(fire, banner.WORDMARK_PIXELS)
    local w, h = #rows[1], math.ceil(#rows / 2)
    return w, h + 1, function(slice, r, c)
      banner.paint_pixels(slice, r, c, rows)
      local pad = math.floor((w - #version) / 2)
      slice:put_str(r + h, c + pad, version, { dim = true })
    end
  end
end

local function paint(slice, _ctx)
  if not STATE then return end
  local w, h, paint_fn = variant_info(PERSIST.variant_idx, PERSIST.frame)
  local sw = slice:width()
  local sh = slice:height()
  local r0 = math.max(0, math.floor((sh - h) / 2))
  local c0 = math.max(0, math.floor((sw - w) / 2))
  paint_fn(slice, r0, c0)
end

local function update_measure()
  if not STATE then return end
  local w, h = variant_info(PERSIST.variant_idx, PERSIST.frame)
  STATE.measure:set(w, h)
end

local function close()
  if not STATE then return end
  if STATE.timer then STATE.timer:remove() end
  if STATE.paint_id then smelt.paint.unregister(STATE.paint_id) end
  if STATE.win then STATE.win:close() end
  if STATE.overlay then STATE.overlay:close() end
  STATE = nil
end

local function cycle(delta)
  PERSIST.variant_idx = ((PERSIST.variant_idx - 1 + delta) % #LABELS) + 1
  PERSIST.frame = 0
  update_measure()
end

local function open()
  if STATE then return end
  STATE = {}

  local w, h = variant_info(PERSIST.variant_idx, PERSIST.frame)
  STATE.measure = smelt.overlay.layout.measure(w, h)

  STATE.buf = smelt.buf.new()
  STATE.buf:lines({ HINT })
  STATE.win = smelt.win.new(STATE.buf, { focusable = true })

  STATE.paint_id = smelt.paint.register(paint)

  STATE.win:key("<Tab>", function() cycle(1) end)
  STATE.win:key("<S-Tab>", function() cycle(-1) end)
  STATE.win:key("<Space>", function() PERSIST.looping = not PERSIST.looping end)
  STATE.win:key("<Esc>", close)
  STATE.win:key("<C-c>", close)

  STATE.overlay = smelt.overlay.new({
    name = "smelt.banner_picker",
    title = {
      { text = " /banner ", fg = "yellow", bold = true },
    },
    anchor = "center",
    border = { all = "Comment" },
    layout = smelt.overlay.layout.vbox({
      {
        smelt.overlay.layout.leaf(STATE.paint_id, { measure = STATE.measure }),
        height = "fit",
      },
      {
        smelt.overlay.layout.leaf(STATE.win, { measure = { #HINT, 1 } }),
        height = 1,
      },
    }, { padding = 2 }),
    modal = false,
    draggable = true,
    resizable = false,
  })
  STATE.win:focus()

  STATE.timer = smelt.timer.every(TICK_MS, function()
    if PERSIST.looping then PERSIST.frame = PERSIST.frame + 1 end
  end)
end

local function toggle()
  if STATE then close() else open() end
end

smelt.cmd.register("banner", toggle, { desc = "logo variant picker (demo)" })

return M
