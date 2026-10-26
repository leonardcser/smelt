-- ~/.config/smelt/plugins/spinner.lua
local fallback = smelt.prompt_bar.top.builtin("indicator")

-- A forge cycle in the status word alone. "working…" types in white-hot at the
-- start of a turn and settles to a glow. Hammer blows sweep across it, each
-- leaving the letters white-hot to cool back down. At the end of a cycle it is
-- quenched into steam, and it decodes back in from embers.
local WORD = { "w", "o", "r", "k", "i", "n", "g", "…" }
local WIDTH = #WORD
local CYCLE_MS = 5200
local DECODE_MS = 700
local FIRST_STRIKE_MS = 1600
local STRIKE_MS = 2200
local SWEEP_STEP_MS = 110
local COOL_TAU_MS = 650
local QUENCH_MS = 600
local FRAME_MS = 110
local TYPE_MS = 45
local GLOW = 0.22
local PEAK = 1
local EMBERS = { "░", "▒", "▓", "█" }
local STEAM = { "·", ".", "'" }

local HOT = { { 150, 40, 25 }, { 235, 110, 30 }, { 255, 200, 70 }, { 255, 245, 225 } }
local COLD = { { 150, 60, 40 }, { 190, 80, 20 }, { 160, 100, 10 }, { 70, 25, 0 } }

local function light() return smelt.theme.is_light() end
local function steel() return light() and { 110, 115, 125 } or { 150, 160, 172 } end

local function mix(a, b, t)
  return {
    math.floor(a[1] + (b[1] - a[1]) * t),
    math.floor(a[2] + (b[2] - a[2]) * t),
    math.floor(a[3] + (b[3] - a[3]) * t),
  }
end

-- Metal color for a heat of 0 (dull red) to 1 (white-hot).
local function metal(heat)
  local stops = light() and COLD or HOT
  local at = math.max(0, math.min(heat, 1)) * (#stops - 1)
  local i = math.min(math.floor(at) + 1, #stops - 1)
  return mix(stops[i], stops[i + 1], at - (i - 1))
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

-- How long ago letter `i` was last struck: when it typed in or settled out of
-- the embers, or when the latest hammer sweep reached it, whichever came later.
local function since_struck(i, length, phase, landed)
  local struck = landed
  local reach = FIRST_STRIKE_MS + (i - 1) * SWEEP_STEP_MS
  if phase >= reach then
    struck = math.max(struck, reach + math.floor((phase - reach) / STRIKE_MS) * STRIKE_MS)
  end
  return phase - struck
end

-- Each turn starts the word typing in, left to right, white-hot.
local started
smelt.prompt_bar.top.add("indicator", function()
  if not working() then
    started = nil
    return fallback()
  end
  local clock = smelt.time.now_ms()
  started = started or clock
  local now = clock - started
  local round = math.floor(now / CYCLE_MS)
  local phase = now % CYCLE_MS
  local frame = math.floor(now / FRAME_MS)
  local quench = (phase - (CYCLE_MS - QUENCH_MS)) / QUENCH_MS

  local typing = round == 0
  local typed = typing and math.min(WIDTH, math.floor(now / TYPE_MS)) or WIDTH

  local runs = {}
  for i = 1, typed do
    local letter = WORD[i]
    local color
    local landed = typing and (i - 1) * TYPE_MS or DECODE_MS * 0.9 * (i / WIDTH)
    if not typing and phase < landed then
      letter = EMBERS[math.floor(noise(i + frame, round) * #EMBERS) + 1]
      color = metal(0.2 + 0.6 * noise(i * 2 + frame, round + 3))
    else
      local age = since_struck(i, WIDTH, phase, landed)
      local heat = GLOW + ((typing and age == phase - landed and 1 or PEAK) - GLOW) * math.exp(-age / COOL_TAU_MS)
      if quench > 0 then
        heat = heat + (1 - heat) * math.min(quench * 2, 1)
        color = mix(metal(heat), steel(), quench * 0.85)
        if noise(i, round) < quench then
          letter = STEAM[math.floor(noise(i + frame, round) * #STEAM) + 1]
          color = mix(steel(), metal(0.2), noise(i + frame, round + 5) * 0.3)
        end
      else
        color = metal(heat)
      end
    end
    runs[#runs + 1] = { text = letter, style = { fg = color, bold = true } }
  end

  return { { runs = runs, priority = 1 } }
end)
