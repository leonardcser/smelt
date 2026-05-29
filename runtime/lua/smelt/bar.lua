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
  local filled = math.floor(ratio * width + 0.5)
  local empty = math.max(0, width - filled)
  local spans = {}

  if opts.boxed ~= false then
    spans[#spans + 1] = { text = opts.left or "[", style = opts.edge_style or { fg = "Comment", dim = true } }
  end
  spans[#spans + 1] = { text = string.rep(opts.filled or "█", filled), style = opts.filled_style }
  spans[#spans + 1] = { text = string.rep(opts.empty or " ", empty), style = opts.empty_style or { bg = { ansi = 237 } } }
  if opts.boxed ~= false then
    spans[#spans + 1] = { text = opts.right or "]", style = opts.edge_style or { fg = "Comment", dim = true } }
  end

  return spans
end

return M
