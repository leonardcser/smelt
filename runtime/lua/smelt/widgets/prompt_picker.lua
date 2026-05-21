-- `smelt.prompt.open_picker(opts)` — prompt-docked picker.
-- Up/Down navigate, Tab inserts the label, Esc dismisses.
--
-- Two modes, distinguished by whether `on_enter` is supplied:
--
--   Single-shot (no on_enter):
--     Enter accepts and closes. Returns `{ index, item, action }` on accept
--     (action: "enter" | "tab"), or `nil` on dismiss.
--
--   Persistent (on_enter is a function):
--     Enter fires `on_enter(item, index)` and the picker stays open.
--     `opts.items` may be a function — it's re-evaluated after each
--     on_enter so callbacks that mutate state (toggle settings, etc.)
--     see the refreshed list without rebuilding the picker. The cursor
--     stays on the same row. Returns nil when the user dismisses.
--
-- opts.items     = { entry, ... } | function() -> { entry, ... }
--   entry = { label, description?, ansi_color?, search_terms? }
-- opts.on_select = function(item)   -- fires on navigation
-- opts.on_enter  = function(item, idx)  -- persistent mode; see above

local function filter_items(all_items, query)
  return smelt.perf.time("picker:filter", function()
    local order = smelt.fuzzy.rank(all_items, query)
    local out = {}
    for i, idx in ipairs(order) do out[i] = all_items[idx] end
    return out
  end)
end

local function to_picker_items(list)
  local out = {}
  for i, it in ipairs(list) do
    out[i] = {
      label       = it.label,
      description = it.description,
      ansi_color  = it.ansi_color,
      label_color = it.label_color,
      prefix      = it.prefix,
    }
  end
  return out
end

local function stamp(original)
  local all = {}
  for i, it in ipairs(original) do
    all[i] = {
      label        = it.label,
      description  = it.description,
      ansi_color   = it.ansi_color,
      prefix       = it.prefix,
      search_terms = it.search_terms,
      _idx         = i,
      _hay         = it._hay
        or ((it.label or "") .. " " .. (it.description or "") .. " " .. (it.search_terms or "")),
    }
  end
  return all
end

local function resolve_items(items)
  return type(items) == "function" and items() or items
end

--- Picker entry shown in the prompt-docked dropdown. `label` and the
--- optional flavour fields mirror what the fuzzy ranker renders; the
--- caller is free to attach extra fields and read them back from
--- `on_select` / `on_enter`.
---@class smelt.prompt.PickerItem
---@field label string Primary text rendered for the row.
---@field description? string Secondary text shown dimmed after the label.
---@field ansi_color? any ANSI color spec used for the prefix glyph.
---@field label_color? any Override the label's color.
---@field prefix? string Glyph rendered before the label.
---@field search_terms? string Extra haystack tokens for the fuzzy match.

--- Options accepted by `smelt.prompt.open_picker`. Passing `on_enter`
--- switches the picker to persistent mode (stays open across selects);
--- omit it for single-shot behaviour.
---@class smelt.prompt.PickerOpts
---@field items smelt.prompt.PickerItem[] | fun(): smelt.prompt.PickerItem[] Eager list or lazy producer.
---@field on_select? fun(item: smelt.prompt.PickerItem): nil Fires on every cursor move.
---@field on_enter? fun(item: smelt.prompt.PickerItem, idx: integer): nil Persistent-mode accept handler.
---@field on_dismiss? fun(): nil Fires on Esc.

-- Prompt-docked picker. Filters `opts.items` (or `opts.items()`) against
-- the current prompt buffer on every keystroke, ranked by `smelt.fuzzy.rank`.
-- Pass `opts.on_select` for the per-navigation hook; pass `opts.on_enter`
-- to switch to persistent mode (the picker stays open across selections
-- until Esc). Returns `{ action, item, index }` on accept or `nil` on
-- dismiss (single-shot mode). Must run inside a `smelt.spawn` frame.
---@type fun(opts: smelt.prompt.PickerOpts): table?
function smelt.prompt.open_picker(opts)
  if not coroutine.isyieldable() then
    error("smelt.prompt.open_picker: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.prompt.open_picker: expected table of options", 2)
  end

  local original = resolve_items(opts.items)
  if type(original) ~= "table" or #original == 0 then
    error("smelt.prompt.open_picker: opts.items must resolve to a non-empty table", 2)
  end

  local on_select = opts.on_select
  local on_enter = opts.on_enter
  local persistent = type(on_enter) == "function"

  local all_items = stamp(original)
  local prompt = smelt.prompt.win()
  local query = smelt.prompt.text() or ""
  local current = query == "" and all_items or filter_items(all_items, query)
  local selected = 1

  local picker = smelt.picker.new({
    items     = to_picker_items(current),
    placement = "prompt_docked",
  })

  -- Claim modal ownership of the prompt so auto-completers (slash / @file /
  -- arg) stay quiet while this picker uses the prompt as its filter input.
  local lock_reg = smelt.prompt.acquire and smelt.prompt.acquire() or nil

  local task_id = smelt.task.alloc()
  local regs = {}

  local function fire_on_select()
    if on_select and current[selected] then
      local orig = original[current[selected]._idx]
      local ok, err = pcall(on_select, orig)
      if not ok then
        smelt.notify.error("prompt picker on_select: " .. tostring(err))
      end
    end
  end
  fire_on_select()

  local function teardown()
    for _, reg in ipairs(regs) do reg:remove() end
    picker:close()
    if lock_reg then lock_reg:remove() end
  end

  local function close_with(result)
    teardown()
    smelt.task.resume(task_id, result)
  end

  local function move(delta)
    local n = #current
    if n == 0 then return end
    selected = ((selected - 1 + delta) % n) + 1
    picker:selected(selected - 1)
    fire_on_select()
  end

  -- Find the position of the logical item with `idx` (1-based original
  -- index) in `current`, or `nil` if it dropped out of the filtered view.
  local function pos_of(idx)
    if not idx then return nil end
    for i, it in ipairs(current) do
      if it._idx == idx then return i end
    end
    return nil
  end

  -- Persistent mode only: refresh items in place after an on_enter callback.
  -- Anchors the cursor to the original item the user was on so reorders
  -- (filter ranking, list shuffles) don't strand the cursor on a different
  -- row. If the item dropped out of the filtered view, falls back to the
  -- nearest still-present position.
  local function refresh()
    local anchor_idx = current[selected] and current[selected]._idx
    original = resolve_items(opts.items)
    all_items = stamp(original or {})
    query = smelt.prompt.text() or ""
    current = query == "" and all_items or filter_items(all_items, query)
    if #current == 0 then
      selected = 1
    else
      selected = pos_of(anchor_idx) or math.min(selected, #current)
    end
    picker:items(to_picker_items(current), selected - 1)
    fire_on_select()
  end

  local function accept(action)
    local picked = current[selected]
    if not picked then
      close_with(nil)
      return
    end
    local idx = picked._idx
    close_with({ action = action, index = idx, item = original[idx] })
  end

  -- Picker renders reversed: index 0 is at the bottom (closest to prompt).
  -- Up moves toward worse matches; Down toward better (closer to prompt).
  regs[#regs + 1] = prompt:key("up",    function() move(1)  end)
  regs[#regs + 1] = prompt:key("down",  function() move(-1) end)
  regs[#regs + 1] = prompt:key("c-k",   function() move(1)  end)
  regs[#regs + 1] = prompt:key("c-j",   function() move(-1) end)
  regs[#regs + 1] = prompt:key("c-p",   function() move(1)  end)
  regs[#regs + 1] = prompt:key("c-n",   function() move(-1) end)
  regs[#regs + 1] = prompt:key("enter", function()
    if persistent then
      local picked = current[selected]
      if not picked then return end
      local orig = original[picked._idx]
      local ok, err = pcall(on_enter, orig, picked._idx)
      if not ok then
        smelt.notify.error("prompt picker on_enter: " .. tostring(err))
        close_with(nil)
        return
      end
      refresh()
    else
      -- Clear prompt before dispatching so the typed query doesn't linger.
      smelt.prompt.set_text("")
      accept("enter")
    end
  end)
  regs[#regs + 1] = prompt:key("tab",   function()
    local picked = current[selected]
    if picked then
      smelt.prompt.set_text(picked.label)
    end
    accept("tab")
  end)
  regs[#regs + 1] = prompt:key("esc",   function() close_with(nil) end)

  regs[#regs + 1] = prompt:on("text_changed", function(ctx)
    query = ctx.text or ""
    current = query == "" and all_items or filter_items(all_items, query)
    -- Reset selection to the top match on each keystroke; the user is
    -- searching, so "best match" beats "stay where you were".
    selected = 1
    picker:items(to_picker_items(current), 0)
    fire_on_select()
  end)

  return smelt.task.wait(task_id)
end
