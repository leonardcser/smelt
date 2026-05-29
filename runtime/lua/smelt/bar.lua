-- Small styled progress-bar helpers shared by Lua commands.

local M = {}

local function clamp(n)
  n = tonumber(n) or 0
  if n < 0 then return 0 end
  if n > 1 then return 1 end
  return n
end

function M.progress(ratio, opts)
  opts = opts or {}
  ratio = clamp(ratio)
  local width = opts.width or 20
  local cells = ratio * width
  local full = math.floor(cells)
  local remainder = cells - full
  local partials = opts.partials or { "▏", "▎", "▍", "▌", "▋", "▊", "▉" }
  local partial = nil

  if full < width and remainder > 0 then
    local idx = math.max(1, math.min(#partials, math.floor(remainder * #partials + 0.5)))
    partial = partials[idx]
  end

  local empty = math.max(0, width - full - (partial and 1 or 0))
  local spans = {}

  if opts.boxed ~= false then
    spans[#spans + 1] = { text = opts.left or "|", style = opts.edge_style or { fg = "Comment", dim = true } }
  end
  spans[#spans + 1] = { text = string.rep(opts.filled or "█", full), style = opts.filled_style }
  if partial then spans[#spans + 1] = { text = partial, style = opts.filled_style } end
  spans[#spans + 1] = { text = string.rep(opts.empty or "░", empty), style = opts.empty_style or { fg = "Comment", dim = true } }
  if opts.boxed ~= false then
    spans[#spans + 1] = { text = opts.right or "|", style = opts.edge_style or { fg = "Comment", dim = true } }
  end

  return spans
end

return M
