-- X-ray: point at a word and every other place it shows on screen lights up with it, fading out
-- for a moment after the mouse moves on.
local fx = require("smelt.fx")

local GLOW = { 255, 196, 70 }
local FADE = 0.6
-- The word last pointed at, where it was, and when the mouse left it.
local lit, paint

local function word_char(c)
  return c ~= nil and c:match("^[%w_]$") ~= nil
end

--- The `(x, y)` of every whole-word copy of `word` on `screen`.
local function find(screen, word)
  local w, ch = screen.w, screen.ch
  local hits = {}
  for y = 0, screen.h - 1 do
    local x = 0
    while x < w do
      if word_char(ch[y * w + x + 1]) then
        local e = x
        while e + 1 < w and word_char(ch[y * w + e + 2]) do e = e + 1 end
        if e - x + 1 == #word and table.concat(ch, "", y * w + x + 1, y * w + e + 1) == word then
          hits[#hits + 1] = { x, y }
        end
        x = e + 1
      else
        x = x + 1
      end
    end
  end
  return hits
end

--- The word under `p` on `screen` and the column it starts at, or nil.
local function word_at(screen, p)
  local w, ch = screen.w, screen.ch
  local at = function(x) return ch[p.y * w + x + 1] end
  if not word_char(at(p.x)) then return nil end
  local a, b = p.x, p.x
  while a > 0 and word_char(at(a - 1)) do a = a - 1 end
  while b < w - 1 and word_char(at(b + 1)) do b = b + 1 end
  local word = table.concat(ch, "", p.y * w + a + 1, p.y * w + b + 1)
  if #word < 3 or word:match("^%d+$") then return nil end
  return word, a
end

paint = smelt.paint.frame(function(s, ctx)
  local now = ctx.now_ms / 1000
  local p = ctx.pointer
  if not p and not lit then return end
  local screen = fx.read(s)
  local word, a = nil, nil
  if p then word, a = word_at(screen, p) end
  local hits = word and find(screen, word)
  if hits and #hits >= 2 then
    lit = { word = word, x = a, y = p.y }
  elseif lit then
    lit.left = lit.left or now
    -- Found again each frame, so a copy that scrolled keeps its light.
    hits = find(screen, lit.word)
  end
  local k = lit and lit.left and 1 - (now - lit.left) / FADE or 1
  if not lit or k <= 0 or #hits < 2 then
    lit = nil
  else
    for _, h in ipairs(hits) do
      local here = not lit.left and h[2] == lit.y and h[1] == lit.x
      local bg = fx.rgb(s:get(h[1], h[2]).style.bg, "bg")
      s:patch_style(h[1], h[2], #lit.word, 1, {
        bg = fx.mix(bg, GLOW, (here and 0.55 or 0.3) * k),
        underline = not here and k > 0.5,
      })
    end
  end
  if lit and lit.left then paint:invalidate_after(33) end
end, { name = "xray", pointer = true })
