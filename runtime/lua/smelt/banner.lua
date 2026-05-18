-- The smelt logo + wordmark art, plus rendering helpers. Pure data and
-- functions; no side effects. Override by reassigning fields (`PALETTE`,
-- `LOGO_PIXELS`, `WORDMARK_PIXELS`) after `require("smelt.banner")` in your
-- own plugin, or `smelt.builtins.disable({ plugins = { "banner" } })` from
-- `early.lua` to drop the bundled empty-state / shutdown surface entirely.
--
-- Pixel coords (capital chars in PALETTE keys, lowercase too) get drawn as
-- the upper / lower half of a `▀` / `▄` cell so two pixel rows pack into
-- one terminal row. `.` is transparent (no cell write at that position).

local M = {}

M.PALETTE = {
  R = 124, -- dark red (outer flame edge)
  O = 202, -- red-orange
  o = 208, -- orange
  Y = 220, -- yellow (hot inner glow)
  L = 223, -- peach (face)
  E = 52,  -- eyes / smile
  W = 15,  -- eye sparkle / wordmark
  G = 244, -- wordmark shadow
}

M.LOGO_PIXELS = {
  "......RR......",
  ".....ROOR.....",
  "..R..RoooR....",
  "..RRRoYYYoR...",
  ".RoYYLLLLYYoR.",
  "RoYLWELLLWEYoR",
  "RoYLELLLLELYoR",
  "RoYYLLEELLYYoR",
  ".RoYYYYYYYYoR.",
  "..RROOOOOORR..",
  "....RRRRRR....",
}

-- 5-glyph "smelt" assembled from a 4-row pixel font with 1-pixel gaps.
M.WORDMARK_PIXELS = {
  "WWW.WWWWW.WWW.W..WWW",
  "WGG.WGWGW.WGG.W..GWG",
  "..W.W.W.W.W...W...W.",
  "WWW.W.W.W.WWW.WW..WW",
}

-- 8x5 mini-flame keyframes for animated wordmark+fire compositions. All
-- frames share dimensions; cycle through them at ~6fps to flicker the
-- flame. Same palette as `LOGO_PIXELS`.
M.MINI_FIRE_FRAMES = {
  {
    "....R...",
    ".R..OR..",
    "..RRoR..",
    ".ROYoOR.",
    "ROYYYoOR",
  },
  {
    "....R...",
    "....OR..",
    ".RRRoR..",
    ".RoYoOR.",
    "ROYYYoOR",
  },
  {
    "....R...",
    "...OOR..",
    "..RRoR..",
    ".ROYoOR.",
    "RoYYYoOR",
  },
}

local function pixel_grid_size(pixels)
  local w = #pixels[1]
  local h = math.ceil(#pixels / 2)
  return w, h
end

function M.wordmark_size() return pixel_grid_size(M.WORDMARK_PIXELS) end
function M.logo_size() return pixel_grid_size(M.LOGO_PIXELS) end

-- Compose the shutdown banner: logo on the left, wordmark vertically
-- centered in the right half. Returns `(rows, version_col, version_row)`
-- where `(version_col, version_row)` is the cell-space anchor for the
-- dimmed program-version label that overlays the bottom of the flame.
function M.compose(logo, wordmark)
  logo = logo or M.LOGO_PIXELS
  wordmark = wordmark or M.WORDMARK_PIXELS
  local gap = 2
  local logo_w = #logo[1]
  local word_w = #wordmark[1]
  -- Keep `top_pad` even so the wordmark's pixel rows pair up at the same
  -- cell-row boundary the logo uses; halving an odd offset would tear the
  -- half-blocks.
  local top_pad = math.floor((#logo - #wordmark) / 2)
  top_pad = top_pad + (top_pad % 2)
  local rows = {}
  for y = 1, #logo do
    local text_y = y - top_pad
    local word_row
    if text_y >= 1 and text_y <= #wordmark then
      word_row = wordmark[text_y]
    else
      word_row = string.rep(".", word_w)
    end
    rows[#rows + 1] = logo[y] .. string.rep(".", gap) .. word_row
  end
  local version_col = logo_w + gap
  local version_row = math.floor((top_pad + #wordmark) / 2)
  return rows, version_col, version_row
end

-- Paint a pixel grid into a `smelt.paint.Slice` using `▀` / `▄` half-block
-- characters. `row0`, `col0` are the cell offsets of the grid's top-left.
function M.paint_pixels(slice, row0, col0, pixels, palette)
  palette = palette or M.PALETTE
  for y = 1, #pixels, 2 do
    local top = pixels[y]
    local bot = pixels[y + 1] or string.rep(".", #top)
    local r = row0 + math.floor((y - 1) / 2)
    for x = 1, #top do
      local fg = palette[top:sub(x, x)]
      local bg = palette[bot:sub(x, x)]
      local c = col0 + x - 1
      if fg and bg then
        slice:set(r, c, "▀", { fg = fg, bg = bg })
      elseif fg then
        slice:set(r, c, "▀", { fg = fg })
      elseif bg then
        slice:set(r, c, "▄", { fg = bg })
      end
    end
  end
end

-- Render a pixel grid to an ANSI-escape string for stdout. `overlays` is
-- an optional list of `{ row, col, text, dim? }` records whose characters
-- replace the pixel cells they cover (used for the dimmed version label).
function M.ansi_render(pixels, palette, overlays)
  palette = palette or M.PALETTE
  local omap = {}
  for _, ov in ipairs(overlays or {}) do
    for i = 1, #ov.text do
      omap[ov.row * 10000 + ov.col + i - 1] = {
        ch = ov.text:sub(i, i),
        dim = ov.dim,
      }
    end
  end
  local out = {}
  for y = 1, #pixels, 2 do
    local top = pixels[y]
    local bot = pixels[y + 1] or string.rep(".", #top)
    local cell_row = math.floor((y - 1) / 2)
    local line = {}
    for x = 1, #top do
      local override = omap[cell_row * 10000 + x - 1]
      if override then
        if override.dim then
          line[#line + 1] = "\27[2m" .. override.ch .. "\27[0m"
        else
          line[#line + 1] = override.ch
        end
      else
        local fg = palette[top:sub(x, x)]
        local bg = palette[bot:sub(x, x)]
        if not fg and not bg then
          line[#line + 1] = " "
        elseif fg and bg then
          line[#line + 1] = string.format("\27[38;5;%d;48;5;%dm▀\27[0m", fg, bg)
        elseif fg then
          line[#line + 1] = string.format("\27[38;5;%dm▀\27[0m", fg)
        else
          line[#line + 1] = string.format("\27[38;5;%dm▄\27[0m", bg)
        end
      end
    end
    out[#out + 1] = table.concat(line)
  end
  return table.concat(out, "\n")
end

return M
