-- ~/.config/smelt/plugins/glitch.lua
local fallback = smelt.prompt_bar.top.builtin("indicator")

-- A glitching status word: each word decodes in from noise, holds with the odd
-- flicker, then corrupts away into the next one. It stays white, with a pink and
-- cyan fringe on the noise.
local WORDS = { "parsing", "linking", "patching", "resolving", "compiling" }
local WIDTH = 9
local CYCLE_MS = 2600
local DECODE_MS = 520
local CORRUPT_MS = 440
local FRAME_MS = 70
local NOISE = { "░", "▒", "▓", "█", "#", "%", "&", "@", "$", "/", "<", ">", "=", "+", "*", "?" }

local function palette()
  if smelt.theme.is_light() then
    return {
      text = { 40, 40, 45 },
      dark = 60,
      bright = 150,
      magenta = { 200, 30, 120 },
      cyan = { 0, 140, 170 },
    }
  end
  return {
    text = { 240, 240, 245 },
    dark = 170,
    bright = 255,
    magenta = { 240, 70, 165 },
    cyan = { 40, 215, 235 },
  }
end

-- Repeatable noise in 0..1, so a frame looks the same however often it repaints.
local function noise(a, b)
  local x = math.sin(a * 12.9898 + b * 78.233) * 43758.5453
  return x - math.floor(x)
end

local function working()
  local state = smelt.signal.get("work_state")
  return state == "working" or state == "retrying" or state == "busy"
end

-- Noise is mostly a random gray; now and then it fringes pink or cyan.
local function static(colors, a, b)
  local pick = noise(a * 3, b + 13)
  if pick < 0.12 then return colors.magenta end
  if pick < 0.24 then return colors.cyan end
  local level = math.floor(colors.dark + (colors.bright - colors.dark) * noise(a, b + 17))
  return { level, level, level }
end

smelt.prompt_bar.top.add("indicator", function()
  if not working() then return fallback() end
  local colors = palette()
  local now = smelt.time.now_ms()
  local round = math.floor(now / CYCLE_MS)
  local word = WORDS[round % #WORDS + 1]
  local phase = now % CYCLE_MS
  local frame = math.floor(now / FRAME_MS)

  -- Now and then the whole word tears into a magenta and cyan split.
  local torn = phase > DECODE_MS and noise(frame, round + 3) > 0.96
  -- A single letter flickers while the word holds.
  local flicker = phase > DECODE_MS and noise(frame, round + 7) > 0.9
    and math.floor(noise(frame, round + 11) * #word) + 1

  local runs = {}
  for i = 1, WIDTH do
    local letter = word:sub(i, i)
    local color = colors.text
    local garbled = false
    if phase < DECODE_MS * 0.9 * (i / #word) then
      garbled = true
    elseif phase > CYCLE_MS - CORRUPT_MS then
      garbled = noise(i, round) < (phase - (CYCLE_MS - CORRUPT_MS)) / CORRUPT_MS
    elseif flicker == i then
      garbled = true
    end
    if letter == "" then
      letter = " "
    elseif garbled then
      letter = NOISE[math.floor(noise(i + frame, round) * #NOISE) + 1]
      color = static(colors, i, frame)
    elseif torn then
      color = noise(i, round) < 0.5 and colors.magenta or colors.cyan
    end
    runs[#runs + 1] = { text = letter, style = { fg = color, bold = true } }
  end

  return { { runs = runs, priority = 1 } }
end)
