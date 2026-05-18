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
-- The overlay is closed + reopened on each Tab so its layout's natural
-- size shrinks to the new variant. The pattern below (PERSIST table, live
-- STATE table, rebuild()) generalizes to any "playground"-style picker
-- where each option has different dimensions.

local banner = require("smelt.banner")

local M = {}

-- Survives close+reopen cycles (variant cycling and Space toggle). Lost on
-- /reload. Use `smelt.state(...)` instead if you want reload-survival.
local PERSIST = { variant_idx = 1, looping = false, frame = 0 }

-- Live resources while the overlay is open. `nil` when closed.
local STATE = nil

local LABELS = {
  "full",
  "full+version",
  "icon",
  "wordmark+version",
  "wordmark+fire",
}

local HINT = " Tab: cycle   Space: loop   Esc: close "
local TICK_MS = 166 -- ~6fps

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
  -- Pad at the top so the wordmark sits flush against the version line.
  if #rows % 2 == 1 then table.insert(rows, 1, string.rep(".", width)) end
  return rows
end

-- Returns (cell_width, cell_height, paint_fn(slice, row0, col0)) for the
-- variant at `idx` and animation frame `frame_idx` (only consumed by
-- wordmark+fire).
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
  local sw, sh = slice:width(), slice:height()
  local r0 = math.max(0, math.floor((sh - h) / 2))
  local c0 = math.max(0, math.floor((sw - w) / 2))
  paint_fn(slice, r0, c0)
end

local close, open

function close()
  if not STATE then return end
  if STATE.timer then STATE.timer:remove() end
  if STATE.paint_id then smelt.paint.unregister(STATE.paint_id) end
  if STATE.win then STATE.win:close() end
  if STATE.overlay then STATE.overlay:close() end
  STATE = nil
end

local function rebuild()
  close()
  open()
end

local function cycle(delta)
  PERSIST.variant_idx = ((PERSIST.variant_idx - 1 + delta) % #LABELS) + 1
  PERSIST.frame = 0
  rebuild()
end

function open()
  if STATE then return end
  STATE = {}

  local w, h = variant_info(PERSIST.variant_idx, PERSIST.frame)
  local inner_w = math.max(w, #HINT)

  STATE.buf = smelt.buf.new()
  STATE.buf:lines({ HINT })
  STATE.win = smelt.win.new(STATE.buf, { focusable = true })

  STATE.paint_id = smelt.paint.register(paint)

  STATE.win:key("<Tab>", function() cycle(1) end)
  STATE.win:key("<S-Tab>", function() cycle(-1) end)
  STATE.win:key("<Space>", function()
    PERSIST.looping = not PERSIST.looping
    rebuild() -- refresh title chip showing LOOP state
  end)
  STATE.win:key("<Esc>", close)
  STATE.win:key("<C-c>", close)

  local title = {
    { text = " /banner ", fg = "yellow", bold = true },
    { text = "(" .. LABELS[PERSIST.variant_idx] .. ")", fg = "grey", dim = true },
  }
  if PERSIST.looping then
    title[#title + 1] = { text = " ● LOOP", fg = "red", dim = true }
  end
  title[#title + 1] = { text = " " }

  STATE.overlay = smelt.overlay.new({
    title = title,
    width = inner_w + 2,
    height = h + 1 + 2, -- art + hint + border
    layout = smelt.overlay.layout.vbox({
      { smelt.overlay.layout.leaf(STATE.paint_id), height = "fill" },
      { smelt.overlay.layout.leaf(STATE.win),      height = 1      },
    }),
    modal = true,
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
