-- Shared spinner glyph and traveling-wave color for plugin animations.
-- Pure Lua - animation logic lives here, not in Rust.

local M = {}

M.SPINNER_FRAMES = { "✿", "❀", "✾", "❁" }
M.SPINNER_FRAME_MS = 150
M.WAVE_PERIOD_MS = 1200
M.WAVE_WAVELENGTH = 16.0
M.WAVE_LOW = 140
M.WAVE_HIGH = 255
M.LIGHT_WAVE_LOW = 90
M.LIGHT_WAVE_HIGH = 185

--- Return the current spinner glyph (single grapheme).
-- Frame selection derives from wall-clock time so multiple processes
-- animate in lockstep without inter-process communication.
function M.glyph()
  local unix_ms = smelt.clock.unix_ms()
  local idx = math.floor(unix_ms / M.SPINNER_FRAME_MS) % #M.SPINNER_FRAMES + 1
  return M.SPINNER_FRAMES[idx]
end

--- Return the spinner frame period in milliseconds.
function M.period_ms()
  return M.SPINNER_FRAME_MS
end

--- Traveling-wave grayscale color for cell offset `x`.
-- Phase is derived from wall-clock time so every instance shares the
-- same temporal sync point; `x` provides the spatial offset.
function M.wave_color_at(x)
  local unix_ms = smelt.clock.unix_ms()
  local t = unix_ms / M.WAVE_PERIOD_MS
  local phase = (t - x / M.WAVE_WAVELENGTH) * 2 * math.pi
  local intensity = (math.sin(phase) + 1.0) * 0.5
  local low, high = M.WAVE_LOW, M.WAVE_HIGH
  local ok, light = pcall(function()
    return smelt.theme.is_light()
  end)
  if ok and light then
    low, high = M.LIGHT_WAVE_LOW, M.LIGHT_WAVE_HIGH
  end
  local level = math.floor(low + (high - low) * intensity + 0.5)
  return { level, level, level }
end

return M
