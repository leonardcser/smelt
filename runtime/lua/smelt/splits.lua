-- Remembered split sizes. A split created with `key` starts at the size the
-- user last dragged it to, else `smelt.settings.splits[key]`, else its `size`
-- option. Every later resize is saved, debounced, to the persistent "splits"
-- state, so the size outlives the view and the process.

local layout = smelt.ui.layout
local create = layout.split
local saved = smelt.state.persistent("splits")

local function with_size(opts, size)
  local copy = {}
  for k, v in pairs(opts) do copy[k] = v end
  copy.size = size
  return copy
end

function layout.split(axis, opts)
  local key = opts and opts.key
  if key == nil then return create(axis, opts) end

  local remembered = saved[key]
  if remembered ~= nil then
    local ok, split = pcall(create, axis, with_size(opts, remembered))
    if ok then return split end
  end
  local configured = smelt.settings.splits[key]
  if configured ~= nil then
    local ok, split = pcall(create, axis, with_size(opts, configured))
    if ok then return split end
    smelt.notify.warn("settings.splits." .. key .. ": " .. tostring(split), "splits")
  end
  return create(axis, opts)
end

smelt.events.on("split_resized", function(event)
  saved[event.key] = event.size
end)
