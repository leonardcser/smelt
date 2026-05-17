-- `smelt.prompt.open_picker(opts)` — prompt-docked picker.
-- Up/Down navigate, Enter accepts, Tab inserts the label, Esc dismisses.
--
-- opts.items    = { { label, description?, ansi_color?, search_terms? }, ... }
-- opts.on_select = function(item)  -- fires on navigation
--
-- Returns `{ index, item, action }` on accept, nil on dismiss.

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
      prefix      = it.prefix,
    }
  end
  return out
end

function smelt.prompt.open_picker(opts)
  if not coroutine.isyieldable() then
    error("smelt.prompt.open_picker: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.prompt.open_picker: expected table of options", 2)
  end
  if type(opts.items) ~= "table" or #opts.items == 0 then
    error("smelt.prompt.open_picker: opts.items must be a non-empty table", 2)
  end

  local original = opts.items
  local on_select = opts.on_select

  -- Stamp each entry with its original index so filtering can resolve back to it.
  -- Precompute `_hay` so per-keystroke ranking skips concatenation.
  local all_items = {}
  for i, it in ipairs(original) do
    all_items[i] = {
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

  local current = all_items
  local selected = 1

  local prompt = smelt.prompt.win()
  local initial_query = smelt.prompt.text() or ""
  if initial_query ~= "" then
    current = filter_items(all_items, initial_query)
  end

  local picker = smelt.picker.new({
    items     = to_picker_items(current),
    placement = "prompt_docked",
  })

  local task_id = smelt.task.alloc()
  local regs = {}

  local function fire_on_select()
    if on_select and current[selected] then
      local orig = original[current[selected]._idx]
      local ok, err = pcall(on_select, orig)
      if not ok then
        smelt.ui.notify_error("prompt picker on_select: " .. tostring(err))
      end
    end
  end
  fire_on_select()

  local function teardown()
    for _, reg in ipairs(regs) do reg:remove() end
    picker:close()
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
  -- Clear prompt before dispatching so the typed query doesn't linger.
  regs[#regs + 1] = prompt:key("enter", function()
    smelt.prompt.set_text("")
    accept("enter")
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
    local query = ctx.text or ""
    current = filter_items(all_items, query)
    selected = 1
    picker:items(to_picker_items(current)):selected(0)
    fire_on_select()
  end)

  return smelt.task.wait(task_id)
end
