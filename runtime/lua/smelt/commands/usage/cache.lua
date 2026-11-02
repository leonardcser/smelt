-- In-memory usage reports. Failed refreshes retain the last successful report.
-- Subscribers receive snapshots; rendering belongs to the dialog.

local M = {}

-- key -> entry
local cache = {}

function M.key(provider, scope)
  return provider .. "\0" .. (scope or "")
end

local function ensure(key)
  cache[key] = cache[key] or {
    refreshing = false,
    subscribers = {},
  }
  return cache[key]
end

local function notify(key)
  local entry = cache[key]
  if not entry then return end
  for callback in pairs(entry.subscribers) do
    local ok, err = pcall(callback, M.get(key))
    if not ok then
      smelt.log.warn("usage_cache_subscriber_failed", { error = tostring(err) })
      entry.subscribers[callback] = nil
    end
  end
end

--- Subscribe `callback(snapshot)` to updates for `key`.
function M.subscribe(key, callback)
  ensure(key).subscribers[callback] = true
end

--- Unsubscribe a previously registered callback.
function M.unsubscribe(key, callback)
  local entry = cache[key]
  if entry then entry.subscribers[callback] = nil end
end

local function fresh(entry)
  if not entry.report or entry.error or entry.refreshing or entry.redeeming
      or smelt.time.monotonic_ms() - entry.fetched_at_ms >= 60000 then return false end
  local credits = entry.report.resetCredits
  local inventory = credits and credits.inventory
  for _, credit in ipairs(type(inventory) == "table" and inventory or {}) do
    if credit.expires_at and credit.expires_at <= smelt.time.now() then return false end
  end
  return true
end

--- Return a snapshot of the cached entry, or nil if none exists.
function M.get(key)
  local entry = cache[key]
  if not entry then return nil end
  return {
    report = entry.report,
    fetched_at_ms = entry.fetched_at_ms,
    error = entry.error,
    refreshing = entry.refreshing,
    redeeming = entry.redeeming,
    fresh = fresh(entry),
  }
end

--- Refresh using `fetch_fn() -> report, error`. Concurrent calls share one fetch.
function M.refresh(key, fetch_fn)
  local entry = ensure(key)
  if entry.refreshing or entry.redeeming then return end
  entry.refreshing = true
  notify(key)

  smelt.spawn(function()
    local ok, report, err = pcall(fetch_fn, key)
    if ok and type(report) == "table" then
      entry.report = report
      if report.resetCredits and report.resetCredits.count == 0 then entry.request_id = nil end
      entry.fetched_at_ms = smelt.time.monotonic_ms()
      entry.error = nil
      entry.retry_at_ms = nil
    else
      entry.error = tostring((ok and err or report) or "usage unavailable - try again later")
      entry.retry_at_ms = smelt.time.monotonic_ms() + 60000
      smelt.log.warn("usage_fetch_failed", { error = entry.error })
    end
    entry.refreshing = false
    if entry.report and not fresh(entry) then entry.retry_at_ms = smelt.time.monotonic_ms() + 60000 end
    notify(key)
  end)
end

--- Reuse reports for one minute; explicit Refresh always fetches.
function M.prefetch(key, fetch_fn)
  local entry = cache[key]
  if entry and (fresh(entry) or (entry.retry_at_ms and smelt.time.monotonic_ms() < entry.retry_at_ms)) then return end
  M.refresh(key, fetch_fn)
end

--- Serialize credit redemption per account. Keep the id after uncertain failures.
function M.redeem(key, redeem_fn, done)
  local previous = ensure(key)
  if not fresh(previous) then return end
  local entry = {
    subscribers = previous.subscribers, redeeming = true,
    request_id = previous.request_id or string.format("smelt-%d-%d-%d", os.time(), math.floor(os.clock() * 1000), math.random(100000000, 999999999)),
  }
  cache[key] = entry
  notify(key)
  smelt.spawn(function()
    local ok, message, err = pcall(redeem_fn, entry.request_id)
    if not ok then err, message = tostring(message), nil end
    entry.redeeming = false
    if message then entry.request_id = nil end
    entry.error = err
    notify(key)
    done(message, err)
  end)
end

return M
