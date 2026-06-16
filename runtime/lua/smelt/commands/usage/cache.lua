-- Background usage cache with subscriber-driven in-place updates.
--
-- Keeps a per-provider cache of the styled provider-usage section. Callers
-- subscribe a callback, trigger refreshes, and receive updates when data
-- arrives. The cache is intentionally module-local: it is rebuilt on `/reload`
-- via the `on_ready` prefetch hook in `usage.lua`.

local M = {}

-- key -> entry
local cache = {}

function M.key(provider, model)
  return (provider ~= "" and provider or model or "unknown")
end

local function ensure(key)
  cache[key] = cache[key] or {
    lines = nil,
    fetched_at = nil,
    error = nil,
    refreshing = false,
    generation = 0,
    subscribers = {},
  }
  return cache[key]
end

local function notify(key)
  local entry = cache[key]
  if not entry then return end
  for callback in pairs(entry.subscribers) do
    local ok, err = pcall(callback, entry.lines, not entry.refreshing, entry.error)
    if not ok then
      smelt.log.warn("usage_cache_subscriber_failed", { key = key, error = tostring(err) })
      entry.subscribers[callback] = nil
    end
  end
end

--- Subscribe `callback(lines, fresh, error_message)` to updates for `key`.
function M.subscribe(key, callback)
  ensure(key).subscribers[callback] = true
end

--- Unsubscribe a previously registered callback.
function M.unsubscribe(key, callback)
  local entry = cache[key]
  if entry then entry.subscribers[callback] = nil end
end

--- Return a snapshot of the cached entry, or nil if none exists.
function M.get(key)
  local entry = cache[key]
  if not entry then return nil end
  return {
    lines = entry.lines,
    fetched_at = entry.fetched_at,
    error = entry.error,
    refreshing = entry.refreshing,
  }
end

--- Refresh `key` in the background using `fetch_fn()` -> styled lines.
-- If a refresh is already in flight, the caller is subscribed to its result.
function M.refresh(key, fetch_fn)
  local entry = ensure(key)
  if entry.refreshing then return end

  entry.refreshing = true
  entry.error = nil
  entry.generation = entry.generation + 1
  local generation = entry.generation
  notify(key)

  smelt.spawn(function()
    local ok, result = pcall(fetch_fn)
    local lines, err
    if ok then
      lines = result
    else
      err = tostring(result)
    end

    -- A newer refresh superseded this one while we were in flight.
    if entry.generation ~= generation then return end

    if lines then
      entry.lines = lines
      entry.fetched_at = os.time()
      entry.error = nil
    elseif err then
      entry.error = err
    end
    entry.refreshing = false
    notify(key)
  end)
end

--- Refresh only if this key has never been fetched successfully.
function M.prefetch(key, fetch_fn)
  local entry = cache[key]
  if entry and entry.lines then return end
  M.refresh(key, fetch_fn)
end

return M
