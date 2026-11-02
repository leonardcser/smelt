-- Shared helpers for the screen effects: colors as the terminal shows them, blending, easing, glyphs
-- turned over, and the text the model is writing, followed as it scrolls.
local M = {}

-- Colors resolved to rgb, by layer and then by color: ANSI slots and the terminal default each
-- cost a call into smelt, and a screen-wide effect asks for the same few on every cell.
local resolved = { fg = {}, bg = {} }

--- `c` (any color a cell holds) as `{ r, g, b }`, the way the terminal shows it on `layer` (`"fg"`
--- by default).
function M.rgb(c, layer)
  if type(c) == "table" then return c end
  layer = layer or "fg"
  local key = c or false
  local hit = resolved[layer][key]
  if not hit then
    hit = smelt.theme.rgb(c, layer)
    resolved[layer][key] = hit
  end
  return hit
end

--- `a` moved toward `b` by `k`.
function M.mix(a, b, k)
  return { a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k, a[3] + (b[3] - a[3]) * k }
end

--- What shows behind a screen that moved away: the terminal's background, darkened.
function M.void()
  return M.mix(M.rgb(nil, "bg"), { 0, 0, 0 }, 0.6)
end

--- A stable pseudo-random number in [0, 1) for `n`.
function M.hash(n)
  local v = math.sin(n * 12.9898) * 43758.5453
  return v - math.floor(v)
end

--- `t` in [0, 1] eased in and out.
function M.ease(t)
  return t < 0.5 and 4 * t * t * t or 1 - (-2 * t + 2) ^ 3 / 2
end

--- Seconds on the clock paints and timers share.
function M.now()
  return smelt.time.monotonic_ms() / 1000
end

--- Whether `(x, y)` is inside `r` (`{ x, y, width, height }`).
function M.inside(r, x, y)
  return x >= r.x and y >= r.y and x < r.x + r.width and y < r.y + r.height
end

--- The glyph of every cell, `ch[y * w + x + 1]`. The right half of a wide glyph reads as "".
function M.read(s)
  local w = s:width()
  local screen = { w = w, h = s:height(), ch = {} }
  local ch = screen.ch
  s:map(function(c, x, y)
    ch[y * w + x + 1] = c.ch
    if c.width == 2 then ch[y * w + x + 2] = "" end
  end)
  return screen
end

--- The conversation's blocks on screen, top to bottom, each with its screen `rect` and `top`, the
--- block row shown at `rect.y`.
function M.blocks()
  local out = {}
  for _, b in ipairs(smelt.transcript.visible_blocks()) do
    if b.rect then out[#out + 1] = b end
  end
  return out
end

--- The screen rows that scrolled into view since the last frame, as a set: they changed because the
--- view moved, not because their text did.
function M.revealed(ctx)
  local rows = {}
  for _, sc in ipairs(ctx.scrolls or {}) do
    for y = sc.y, sc.y + sc.height - 1 do
      if y + sc.dy < sc.y or y + sc.dy >= sc.y + sc.height then rows[y] = true end
    end
  end
  return rows
end

--- A tracker of the text the model is writing: call it each frame of a paint with `history`, and it
--- returns `{ x, y, t }` for every glyph written in the last `life` seconds, `t` when it appeared.
--- Text keeps its age as it scrolls, and nothing counts as new outside a streaming block.
function M.fresh(life)
  -- Per block id: the deepest row shown so far, and when glyphs appeared, by row and then column.
  local seen = {}
  return function(s, ctx)
    local now = ctx.now_ms / 1000
    local rows, live = {}, {}
    for _, b in ipairs(M.blocks()) do
      local st = seen[b.block_id] or { deep = -1, born = {} }
      live[b.block_id] = st
      st.rect, st.top = b.rect, b.top
      if b.streaming then
        for y = b.rect.y, b.rect.y + b.rect.height - 1 do rows[y] = st end
      end
    end
    seen = live
    local revealed = M.revealed(ctx)
    for _, d in ipairs(ctx.damage or {}) do
      local st = rows[d.y]
      local row = st and st.top + d.y - st.rect.y
      -- A row that scrolled into view is new only past anything shown before: the view following
      -- the model down.
      if st and (not revealed[d.y] or row > st.deep) then
        local r = st.rect
        for x = math.max(d.x, r.x), math.min(d.x + d.width, r.x + r.width) - 1 do
          local ch = s:get(x, d.y).ch
          if ch ~= " " and ch ~= "" then
            st.born[row] = st.born[row] or {}
            st.born[row][x] = now
          end
        end
      end
    end
    for _, st in pairs(seen) do st.deep = math.max(st.deep, st.top + st.rect.height - 1) end
    local out = {}
    for _, st in pairs(seen) do
      for row, born in pairs(st.born) do
        local y = st.rect.y + row - st.top
        for x, t in pairs(born) do
          if now - t >= life then
            born[x] = nil
          elseif row >= st.top and row < st.top + st.rect.height then
            out[#out + 1] = { x = x, y = y, t = t }
          end
        end
        if next(born) == nil then st.born[row] = nil end
      end
    end
    return out
  end
end

--- Glyphs turned half a turn: letters upside down, box corners and half blocks swapped, braille
--- dots turned. A glyph with no turned form stays as it is.
M.TURN = {}
for _, pair in ipairs({
  "aɐ", "bq", "cɔ", "dp", "eǝ", "fɟ", "gƃ", "hɥ", "iᴉ", "jɾ", "kʞ", "mɯ", "nu", "rɹ", "tʇ", "vʌ", "wʍ", "yʎ",
  "A∀", "Bꓭ", "CƆ", "Dᗡ", "EƎ", "FℲ", "G⅁", "Jſ", "Kꓘ", "L˥", "MW", "PԀ", "QΌ", "Rᴚ", "T⊥", "U∩", "VΛ", "Y⅄",
  "1Ɩ", "3Ɛ", "5ϛ", "69", ".˙", ",'", "!¡", "?¿", "()", "[]", "{}", "<>", "_‾", "&⅋",
  "┌┘", "┐└", "├┤", "┬┴", "╭╯", "╮╰", "▀▄", "▌▐", "▖▝", "▗▘", "▔▁", "↑↓", "←→", "▲▼", "▶◀",
}) do
  local a, b = pair:match("^([%z\1-\127\194-\244][\128-\191]*)(.+)$")
  M.TURN[a], M.TURN[b] = b, a
end
-- Braille dots by bit, each to the bit of the dot opposite it.
local DOT = { [0] = 7, 5, 4, 6, 2, 1, 3, 0 }
for bits = 1, 255 do
  local turned = 0
  for i = 0, 7 do
    if bits & (1 << i) ~= 0 then turned = turned | (1 << DOT[i]) end
  end
  M.TURN[utf8.char(0x2800 + bits)] = utf8.char(0x2800 + turned)
end

--- Glyphs mirrored top to bottom, as in water: letters whose mirror image is another letter, and
--- lines, corners and half blocks. Any other glyph stays as it is.
M.MIRROR = {}
for _, pair in ipairs({
  "bp", "dq", "nu", "mw", "MW", "A∀", "VΛ", "T⊥", "'‚", "^v", "_‾",
  "▀▄", "┌└", "┐┘", "╭╰", "╮╯", "┬┴", "▔▁", "↑↓", "▲▼",
}) do
  local a, b = pair:match("^([%z\1-\127\194-\244][\128-\191]*)(.+)$")
  M.MIRROR[a], M.MIRROR[b] = b, a
end

--- Glyphs turned a quarter turn clockwise: lines, corners, half blocks and arrows. Letters have no
--- turned forms, so they stay upright.
M.QUARTER = {}
--- The same a quarter turn counterclockwise.
M.QUARTER_BACK = {}
for _, ring in ipairs({
  "─│", "━┃", "-|", "/\\", "╱╲", "…⋮", "┌┐┘└", "╭╮╯╰", "├┬┤┴", "▀▐▄▌", "↑→↓←", "▲▶▼◀",
}) do
  local g = {}
  for _, c in utf8.codes(ring) do g[#g + 1] = utf8.char(c) end
  for i, c in ipairs(g) do
    local next = g[i % #g + 1]
    M.QUARTER[c], M.QUARTER_BACK[next] = next, c
  end
end

--- Cover the whole screen with `paint` until a key is pressed, then call `on_close`. Returns a
--- function that puts the cover away.
function M.cover(paint, on_close)
  local input = smelt.input.new({ scrollbar = false })
  local overlay
  local function close()
    if not overlay then return end
    overlay:close()
    overlay = nil
    if on_close then on_close() end
  end
  -- Typing, Enter and Esc reach the empty input; the keys that don't type anything are bound.
  for _, event in ipairs({ "change", "submit", "cancel" }) do input:on(event, close) end
  local keymaps = {}
  for _, key in ipairs({ "<Up>", "<Down>", "<Left>", "<Right>", "<Tab>", "<BS>", "<C-c>" }) do
    keymaps[#keymaps + 1] = { key = key, on_press = close }
  end
  local layout = smelt.ui.layout
  overlay = smelt.overlay.new({
    anchor = "screen_at", row = 0, col = 0, width = "100%", height = "100%", border = "none", z = 300,
    layout = layout.vbox({
      { layout.leaf(input:win()), height = 0 },
      { layout.leaf(paint), height = "fill" },
    }),
    keymaps = keymaps,
  })
  input:win():focus()
  return close
end

return M
