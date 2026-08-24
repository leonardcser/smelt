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

local function theme_is_light()
  return smelt.theme.is_light()
end

--- Return the current spinner glyph (single grapheme).
-- Frame selection derives from wall-clock time so multiple processes
-- animate in lockstep without inter-process communication.
function M.glyph()
  local unix_ms = smelt.time.now_ms()
  local idx = math.floor(unix_ms / M.SPINNER_FRAME_MS) % #M.SPINNER_FRAMES + 1
  return M.SPINNER_FRAMES[idx]
end

--- Return the spinner frame period in milliseconds.
function M.period_ms()
  return M.SPINNER_FRAME_MS
end

--- Capture the shared traveling-wave phase and theme bounds for one frame.
-- Returning scalars lets renderers color many cells without allocating one
-- state table or repeating host calls per cell.
function M.wave_state()
  local t = smelt.time.now_ms() / M.WAVE_PERIOD_MS
  local ok, light = pcall(theme_is_light)
  if ok and light then
    return t, M.LIGHT_WAVE_LOW, M.LIGHT_WAVE_HIGH
  end
  return t, M.WAVE_LOW, M.WAVE_HIGH
end

--- Return one grayscale level for cell offset `x` in a captured wave state.
function M.wave_level_at(x, t, low, high)
  local phase = (t - x / M.WAVE_WAVELENGTH) * 2 * math.pi
  local intensity = (math.sin(phase) + 1.0) * 0.5
  return math.floor(low + (high - low) * intensity + 0.5)
end

--- Traveling-wave grayscale color for cell offset `x`.
function M.wave_color_at(x)
  local t, low, high = M.wave_state()
  local level = M.wave_level_at(x, t, low, high)
  return { level, level, level }
end

return M
