local internal = __smelt_internal

-- Lifecycle guards capture stable app epochs and let async callbacks cheaply
-- ignore stale completions after a session/history/input boundary changes.
if smelt.lifecycle and smelt.signal then
  local epoch_cells = {
    session = "session_epoch",
    history = "history_epoch",
    input = "input_epoch",
  }
  local lifecycle_latest = {}

  local function normalize_guard_scopes(scopes)
    local out = {}
    local function add(scope)
      if type(scope) ~= "string" or scope == "" then return end
      table.insert(out, epoch_cells[scope] or scope)
    end

    if scopes == nil then
      return out
    end
    if type(scopes) == "string" then
      add(scopes)
      return out
    end
    if type(scopes) ~= "table" then
      error("smelt.lifecycle.guard: scopes must be a string or table", 3)
    end

    for k, v in pairs(scopes) do
      if type(k) == "number" then
        add(v)
      elseif v then
        add(k)
      end
    end
    return out
  end

  --- Cancellation and supersession guard returned by `smelt.lifecycle.guard`.
  ---@class smelt.lifecycle.Guard
  ---@field alive fun(self:smelt.lifecycle.Guard):boolean Return true while every captured epoch still matches and the guard was not cancelled or superseded.
  ---@field cancel fun(self:smelt.lifecycle.Guard) Mark the guard stale immediately.
  ---@field latest fun(self:smelt.lifecycle.Guard,key:string):smelt.lifecycle.Guard Mark this guard as the latest request for `key`; older guards with the same key become stale.
  ---@field wrap fun(self:smelt.lifecycle.Guard,fn:function):function Return a wrapper that calls `fn` only while the guard is alive.
  ---Create a guard whose `:alive()` flips false when any scoped epoch changes.
  ---Scopes are `"session"`, `"history"`, `"input"`, or a concrete signal name.
  ---Use `:latest(key)` when only the newest request in a family may complete.
  ---@type fun(scopes: string|string[]|table?): smelt.lifecycle.Guard
  function smelt.lifecycle.guard(scopes)
    local snapshot = {}
    for _, signal_name in ipairs(normalize_guard_scopes(scopes)) do
      snapshot[signal_name] = smelt.signal.get(signal_name)
    end

    local active = true
    local latest_key = nil
    local latest_token = nil
    local guard = {}

    function guard:alive()
      if not active then return false end
      if latest_key and lifecycle_latest[latest_key] ~= latest_token then return false end
      for signal_name, value in pairs(snapshot) do
        if smelt.signal.get(signal_name) ~= value then return false end
      end
      return true
    end

    function guard:cancel()
      active = false
    end

    function guard:latest(key)
      latest_key = tostring(key)
      latest_token = {}
      lifecycle_latest[latest_key] = latest_token
      return self
    end

    function guard:wrap(fn)
      return function(...)
        if self:alive() then
          return fn(...)
        end
      end
    end

    return guard
  end
end

-- `smelt.engine.ask({ guard = ... })` suppresses stale callbacks centrally.
if smelt.engine and smelt.lifecycle then
  internal.__raw_engine_ask = internal.__raw_engine_ask or smelt.engine.ask
  internal.__raw_engine_ask_inherited = internal.__raw_engine_ask_inherited or smelt.engine.ask_inherited
  local raw_engine_ask = internal.__raw_engine_ask
  local raw_engine_ask_inherited = internal.__raw_engine_ask_inherited

  local function guarded_ask(raw, spec)
    if type(spec) == "table" and spec.guard then
      local guard = spec.guard
      local wrapped = {}
      for k, v in pairs(spec) do
        if k ~= "guard" then wrapped[k] = v end
      end
      for _, name in ipairs({ "on_response", "on_delta", "on_draft_rejected" }) do
        local callback = spec[name]
        if type(callback) == "function" then
          wrapped[name] = function(...)
            if guard:alive() then
              return callback(...)
            end
          end
        end
      end
      spec = wrapped
    end
    return raw(spec)
  end

  ---@tier ui_host
  smelt.engine.ask = function(spec)
    return guarded_ask(raw_engine_ask, spec)
  end

  ---@tier ui_host
  smelt.engine.ask_inherited = function(spec)
    return guarded_ask(raw_engine_ask_inherited, spec)
  end
end
