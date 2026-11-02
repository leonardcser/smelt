-- Prompt-docked implementation used by `smelt.picker.open(opts)`.
-- Up/Down navigate, Tab inserts the label, Esc/Ctrl-C dismiss.
--
-- Two modes, distinguished by whether `on_enter` is supplied:
--
--   Single-shot (no on_enter):
--     Enter accepts and closes. Returns `{ index, item, action }` on accept
--     (action: "enter" | "tab"), or `nil` on dismiss.
--
--   Persistent (on_enter is a function):
--     Enter fires `on_enter(item, index)` and the picker stays open.
--     `opts.items` may be a function - it's re-evaluated after each
--     on_enter so callbacks that mutate state (toggle settings, etc.)
--     see the refreshed list without rebuilding the picker. The cursor
--     stays on the same row. Returns nil when the user dismisses.
--
-- opts.items     = { entry, ... } | function() -> { entry, ... }
--   entry = { label, description?, prefix_style?, search_terms? }
-- opts.on_select = function(item)   -- fires on navigation
-- opts.on_enter  = function(item, idx)  -- persistent mode; see above
-- opts.rank      = function(items, query) -> { idx, ... }
--   Returns 1-based indices into `items` in display order; missing/invalid
--   indices are ignored.
-- opts.provider  = function(query, limit) -> { items, searching?, scanning?, message?, status? }
--   Provider results use the same contract as completers. While loading, the
--   picker keeps stale rows and polls until fresh rows arrive.

local function search_keys(items)
  local keys = {}
  for i, item in ipairs(items) do
    keys[i] = type(item) == "string" and item
      or item._hay or ((item.label or "") .. " " .. (item.description or "") .. " " .. (item.search_terms or ""))
  end
  return keys
end

local function filter_items(items, query, rank, keys)
  if query == "" and not rank then return items, nil end
  local order = smelt.perf.time("picker:rank", function()
    if rank then return rank(items, query) or {} end
    return smelt.fuzzy.rank(keys, query)
  end)
  local rows, indices = {}, {}
  for _, idx in ipairs(order) do
    if type(idx) == "number" and items[idx] then
      rows[#rows + 1] = items[idx]
      indices[#indices + 1] = idx
    end
  end
  return rows, indices
end

local picker_rows = require("smelt.widgets.picker_rows")

local function synthetic(item)
  return type(item) == "table" and item._synthetic
end

local function resolve_items(items)
  return type(items) == "function" and items() or items
end

--- High-level picker options. Static floating pickers accept `items` and
--- `placement`. Prompt-docked pickers also support ranking, providers, and
--- persistent `on_enter` handling.
---@class smelt.picker.OpenOpts
---@field items? (string|smelt.picker.Item)[] | fun(): (string|smelt.picker.Item)[] Eager list or lazy producer.
---@field placement? "center"|"bottom"|"cursor"|"prompt_docked" Picker placement. Ranking, providers, and persistent mode use `prompt_docked`.
---@field provider? fun(query: string, limit: integer): table Async provider returning `{ items, searching?, scanning?, message?, status? }`.
---@field limit? integer Maximum rows requested from `provider`; defaults to 200.
---@field poll_ms? integer Refresh interval while provider returns `{ scanning = true }` or `{ searching = true }`.
---@field loading_delay_ms? integer Delay before showing an initial loading row when there are no stale rows to keep.
---@field loading_poll_ms? integer Quiet polling interval before the initial loading row appears.
---@field selected? integer Initial selected row (1-based, clamped); defaults to 1.
---@field on_select? fun(item: string|smelt.picker.Item): nil Fires on every cursor move.
---@field render? fun(item: any): string|smelt.picker.Item Format a display row without changing the original item passed to callbacks. Not called for loading/error rows.
---@field on_enter? fun(item: string|smelt.picker.Item, idx: integer): nil Persistent-mode accept handler.
---@field rank? fun(items: (string|smelt.picker.Item)[], query: string): integer[] Rank the original items. Return 1-based indices in display order; invalid indices are ignored.
---@field on_dismiss? fun(): nil Fires on Esc/Ctrl-C.

function __smelt_internal.picker.open_prompt(opts)
  if not coroutine.isyieldable() then
    error("smelt.picker.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.picker.open: expected table of options", 2)
  end

  local on_select = opts.on_select
  local on_enter = opts.on_enter
  local rank = opts.rank
  if rank ~= nil and type(rank) ~= "function" then
    error("smelt.picker.open: opts.rank must be a function", 2)
  end
  local provider_fn = opts.provider
  if provider_fn ~= nil and type(provider_fn) ~= "function" then
    error("smelt.picker.open: opts.provider must be a function", 2)
  end
  if provider_fn == nil and opts.items == nil then
    error("smelt.picker.open: opts.items or opts.provider required", 2)
  end
  local persistent = type(on_enter) == "function"

  local prompt = smelt.prompt.win
  local query = smelt.prompt.text() or ""
  local limit = opts.limit or 200
  local provider_state = nil

  local function read_provider(show_message)
    local normalized = smelt.provider.normalize(provider_fn(query, limit), {
      show_message = show_message,
      loading_message = opts.loading_message or "searching…",
    })
    return normalized.rows, normalized
  end

  local function resolve_initial()
    if not provider_fn then return resolve_items(opts.items), nil end
    local tick_ms = opts.loading_poll_ms or 50
    local delay_ms = opts.loading_delay_ms or 150
    local elapsed = 0
    while true do
      local rows, normalized = read_provider(elapsed >= delay_ms)
      if #rows > 0 or not normalized.loading then return rows, normalized end
      smelt.sleep(tick_ms)
      elapsed = elapsed + tick_ms
    end
  end

  local original, initial_provider_state = resolve_initial()
  if type(original) ~= "table" or #original == 0 then
    error("smelt.picker.open: opts.items or opts.provider must resolve to a non-empty table", 2)
  end

  provider_state = initial_provider_state
  local keys = not provider_fn and not rank and search_keys(original) or nil
  local current, indices = original, nil
  if not provider_fn then current, indices = filter_items(original, query, rank, keys) end
  local selected = math.max(1, math.min(#current, math.floor(tonumber(opts.selected) or 1)))

  local function original_index()
    return indices and indices[selected] or selected
  end
  local picker = smelt.picker.new({
    items     = picker_rows(current, opts),
    placement = "prompt_docked",
  })
  picker:selected(selected - 1)

  -- Claim modal ownership of the prompt so auto-completers (slash / @file /
  -- arg) stay quiet while this picker uses the prompt as its filter input.
  local lock_reg = smelt.prompt.acquire and smelt.prompt.acquire() or nil
  local lifecycle = __smelt_internal.picker.new_lifecycle(picker, opts)
  lifecycle:add(lock_reg)

  local function fire_on_select()
    local item = current[selected]
    if item and not synthetic(item) then lifecycle:call("on_select", on_select, item) end
  end
  fire_on_select()

  local function move(delta)
    local n = #current
    if n == 0 then return end
    selected = ((selected - 1 + delta) % n) + 1
    picker:selected(selected - 1)
    fire_on_select()
  end

  local provider_poll_reg = nil
  local ensure_provider_poll

  local function stop_provider_poll()
    lifecycle:remove(provider_poll_reg)
    provider_poll_reg = nil
  end

  local function apply_provider_result(rows, normalized, reset_to_top)
    if __smelt_internal.provider.should_keep_stale_rows(normalized, rows, current) then
      provider_state = normalized
      ensure_provider_poll()
      return
    end

    local old_key = (not reset_to_top) and smelt.provider.item_key(current[selected]) or nil
    original = rows or {}
    current, indices = original, nil
    provider_state = normalized

    if #current == 0 then
      selected = 1
    else
      local fallback = reset_to_top and 1 or math.min(selected, #current)
      selected = __smelt_internal.provider.select_row(current, old_key, not reset_to_top, fallback)
    end

    picker:items(picker_rows(current, opts), selected - 1)
    fire_on_select()

    if normalized.loading then
      ensure_provider_poll()
    else
      stop_provider_poll()
    end
  end

  ensure_provider_poll = function()
    if not (provider_fn and smelt.timer and provider_state and provider_state.loading) then return end
    if provider_poll_reg then return end
    provider_poll_reg = smelt.timer.every(opts.poll_ms or 150, function()
      if not (provider_state and provider_state.loading) then
        stop_provider_poll()
        return
      end
      query = smelt.prompt.text() or ""
      local rows, normalized = read_provider(true)
      apply_provider_result(rows, normalized, false)
    end)
    lifecycle:add(provider_poll_reg)
  end

  local function apply_provider_rows(show_message, reset_to_top)
    local rows, normalized = read_provider(show_message)
    apply_provider_result(rows, normalized, reset_to_top)
  end

  ensure_provider_poll()

  -- Persistent mode only: refresh items in place after an on_enter callback.
  -- Anchors the cursor to the original item the user was on so reorders
  -- (filter ranking, list shuffles) don't strand the cursor on a different
  -- row. If the item dropped out of the filtered view, falls back to the
  -- nearest still-present position.
  local function refresh()
    if provider_fn then
      apply_provider_rows(true, false)
      return
    end

    local old_key = smelt.provider.item_key(current[selected])
    original = resolve_items(opts.items) or {}
    keys = not rank and search_keys(original) or nil
    query = smelt.prompt.text() or ""
    current, indices = filter_items(original, query, rank, keys)
    selected = __smelt_internal.provider.select_row(current, old_key, true, math.max(1, math.min(selected, #current)))
    picker:items(picker_rows(current, opts), selected - 1)
    fire_on_select()
  end

  local function accept(action, item, index)
    lifecycle:close(item and { action = action, index = index, item = item } or nil)
  end

  -- Picker renders reversed: index 0 is at the bottom (closest to prompt).
  -- Up moves toward worse matches; Down toward better (closer to prompt).
  lifecycle:add(prompt:key("up",    function() move(1)  end))
  lifecycle:add(prompt:key("down",  function() move(-1) end))
  lifecycle:add(prompt:key("c-k",   function() move(1)  end))
  lifecycle:add(prompt:key("c-j",   function() move(-1) end))
  lifecycle:add(prompt:key("c-p",   function() move(1)  end))
  lifecycle:add(prompt:key("c-n",   function() move(-1) end))
  lifecycle:add(prompt:key("enter", function()
    if persistent then
      local picked = current[selected]
      if not picked or synthetic(picked) then return end
      if not lifecycle:call("on_enter", on_enter, picked, original_index()) then
        lifecycle:close(nil)
        return
      end
      refresh()
    else
      local item, index = current[selected], original_index()
      if synthetic(item) then return end
      if item then smelt.prompt.set_text("") end
      accept("enter", item, index)
    end
  end))
  lifecycle:add(prompt:key("tab", function()
    local item, index = current[selected], original_index()
    if synthetic(item) then return end
    if item then smelt.prompt.set_text(type(item) == "string" and item or item.label) end
    accept("tab", item, index)
  end))
  lifecycle:add(prompt:key("esc", function() lifecycle:dismiss() end))
  lifecycle:add(prompt:key("c-c", function() lifecycle:dismiss() end))

  lifecycle:add(prompt:on("text_changed", function(ctx)
    query = ctx.text or ""
    if provider_fn then
      apply_provider_rows(false, true)
      return
    end

    current, indices = filter_items(original, query, rank, keys)
    -- Reset selection to the top match on each keystroke; the user is
    -- searching, so "best match" beats "stay where you were".
    selected = 1
    picker:items(picker_rows(current, opts), 0)
    fire_on_select()
  end))

  return lifecycle:wait()
end
