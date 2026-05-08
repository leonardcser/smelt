-- Implements `smelt.ui.dialog.open(opts)` and `smelt.ui.dialog.open_handle(opts)`.

local M = {}

smelt.ui.dialog = smelt.ui.dialog or {}

-- Panel handle: exposes `.buf`, `.leaf`, `:focus()`, and `:text()` (input panels only).
local function make_panel(spec, leaf)
  local self = { kind = spec.kind, name = spec.name, buf = spec.buf, leaf = leaf }
  function self:focus()
    if self.leaf then smelt.win.set_focus(self.leaf) end
  end
  if spec.kind == "input" then
    function self:text()
      if not self.leaf then return "" end
      local buf = smelt.win.buf(self.leaf)
      if not buf then return "" end
      return smelt.buf.get_line(buf, 1) or ""
    end
  end
  return self
end

-- Build the dialog handle. `panels` is keyed by 1-based index and by `spec.name`.
local function make_handle(win_id, opts, leaves)
  leaves = leaves or {}
  local panels = {}
  if type(opts.panels) == "table" then
    for i, spec in ipairs(opts.panels) do
      if type(spec) == "table" then
        local h = make_panel(spec, leaves[i])
        panels[i] = h
        if spec.name then panels[spec.name] = h end
      end
    end
  end
  local self = { win = win_id, panels = panels }
  function self:focus(name_or_idx)
    local p = panels[name_or_idx]
    if p then p:focus() end
  end
  function self:close() smelt.win.close(win_id) end
  return self
end

-- Synchronous open; returns the typed handle. Prefer `open(opts)` for coroutine-style use.
function smelt.ui.dialog.open_handle(opts)
  if type(opts) ~= "table" then
    error("smelt.ui.dialog.open_handle: expected table of options", 2)
  end
  local win_id, leaves = M._open(opts)
  if type(win_id) ~= "number" then return nil end
  return make_handle(win_id, opts, leaves)
end

local function split_lines(text)
  if text == "" then return { "" } end
  local out = {}
  for line in tostring(text):gmatch("([^\n]*)\n?") do
    if line == "" and #out > 0 and out[#out] == "" then break end
    table.insert(out, line)
  end
  if #out == 0 then out = { "" } end
  return out
end

local NS_PLACEHOLDER = smelt.buf.create_namespace("smelt.dialog.placeholder")

local function make_input_buffer(placeholder)
  local buf = smelt.buf.create()
  if placeholder and placeholder ~= "" then
    smelt.buf.set_lines(buf, { placeholder })
    smelt.buf.set_extmark(buf, NS_PLACEHOLDER, 1, 0, { end_col = #placeholder, dim = true })
  else
    smelt.buf.set_lines(buf, { "" })
  end
  return buf
end

local function make_options_buffer(items)
  local lines = {}
  for _, item in ipairs(items or {}) do
    table.insert(lines, item.label or "")
  end
  if #lines == 0 then lines = { "" } end
  local buf = smelt.buf.create()
  smelt.buf.set_lines(buf, lines)
  return buf
end

local function make_content_buffer(spec)
  local mode = spec.mode
  if spec.kind == "markdown" and not mode then
    mode = "markdown"
  end
  local buf = mode and smelt.buf.create({ mode = mode }) or smelt.buf.create()
  local text = spec.text or ""
  if mode then
    smelt.buf.set_source(buf, text)
  else
    smelt.buf.set_lines(buf, split_lines(text))
  end
  return buf
end

function M._open(opts)
  local panels = opts.panels or {}
  if #panels == 0 then
    error("smelt.ui.dialog.open: panels must be non-empty", 2)
  end

  local leaves = {}
  local overlay_items = {}
  local root = nil
  local initial_focus = nil

  for i, spec in ipairs(panels) do
    local kind = spec.kind
    local buf = spec.buf
    if not buf then
      if kind == "content" or kind == "markdown" then
        buf = make_content_buffer(spec)
      elseif kind == "options" then
        buf = make_options_buffer(spec.items or {})
      elseif kind == "input" then
        buf = make_input_buffer(spec.placeholder)
      elseif kind == "list" then
        error("smelt.ui.dialog.open: list panel requires buf", 2)
      else
        error("smelt.ui.dialog.open: unknown panel kind `" .. tostring(kind) .. "`", 2)
      end
    end

    local focusable = (kind == "list") or (kind == "options") or (kind == "input")
    if spec.focusable ~= nil then focusable = spec.focusable end
    if spec.interactive then focusable = true end

    local leaf = smelt.win.open(buf, {
      region = "dialog_overlay",
      focusable = focusable,
      vim_enabled = spec.interactive or false,
    })
    if not leaf then
      error("smelt.ui.dialog.open: failed to create window", 2)
    end

    if kind == "list" then
      smelt.win.configure_list(leaf, 0)
    elseif kind == "options" then
      local selected = tonumber(spec.selected or 1) or 1
      if selected < 1 then selected = 1 end
      smelt.win.configure_list(leaf, selected - 1)
    elseif kind == "input" then
      smelt.win.configure_input(leaf)
    end

    leaves[i] = leaf
    overlay_items[i] = {
      win = leaf,
      height = spec.height,
      collapse_when_empty = spec.collapse_when_empty or false,
    }
    if not root then root = leaf end
    if spec.focus and not initial_focus then
      initial_focus = leaf
    elseif not initial_focus and (kind == "list" or kind == "options" or kind == "input") then
      initial_focus = leaf
    end
  end

  smelt.ui.overlay.open({
    title = opts.title,
    placement = opts.placement,
    placement_height = opts.placement_height,
    border = opts.border,
    blocks_agent = opts.blocks_agent,
    modal = true,
    items = overlay_items,
  })
  smelt.win.set_focus(initial_focus or root)
  return root, leaves
end

-- Collect live text from all input panels (`name → leaf_win_id`).
local function collect_inputs(input_leaves)
  local out = {}
  for name, leaf in pairs(input_leaves) do
    local buf = smelt.win.buf(leaf)
    out[name] = (buf and smelt.buf.get_line(buf, 1)) or ""
  end
  return out
end

-- Build the keymap ctx `{ selected_index, inputs, close, win }` from a raw callback ctx.
local function build_ctx(raw_ctx, win_id, task_id, selected_idx, input_leaves)
  local ctx = { win = raw_ctx.win }
  if selected_idx then ctx.selected_index = selected_idx end
  ctx.inputs = collect_inputs(input_leaves)
  ctx.close = function()
    smelt.win.close(win_id)
    smelt.task.resume(task_id, {
      action = "dismiss",
      inputs = {},
    })
  end
  return ctx
end

function smelt.ui.dialog.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.ui.dialog.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.ui.dialog.open: expected table of options", 2)
  end

  local option_panel_idx = nil
  local options_meta = {}    -- 1-based option index → {action, on_select}
  local input_on_change = {} -- panel index → on_change fn
  local input_names = {}     -- 1-based panel index → name
  if type(opts.panels) == "table" then
    for i, p in ipairs(opts.panels) do
      if type(p) == "table" then
        if p.kind == "options" and not option_panel_idx then
          option_panel_idx = i
          if type(p.items) == "table" then
            for j, item in ipairs(p.items) do
              options_meta[j] = {
                action = item.action or "select",
                on_select = item.on_select,
              }
            end
          end
        elseif p.kind == "list" and not option_panel_idx then
          option_panel_idx = i
        elseif p.kind == "input" then
          input_names[i] = p.name or ("input_" .. i)
          if type(p.on_change) == "function" then
            input_on_change[i] = p.on_change
          end
        end
      end
    end
  end

  local win_id, leaves = M._open(opts)
  if type(win_id) ~= "number" then
    return { action = "dismiss", inputs = {} }
  end

  local input_leaves = {}
  for i, name in pairs(input_names) do
    local leaf = leaves and leaves[i]
    if leaf then input_leaves[name] = leaf end
  end

  local task_id = smelt.task.alloc()

  smelt.win.on_event(win_id, "submit", function(raw_ctx)
    local idx1 = raw_ctx.index
    local action = "select"
    local on_select_fn = nil
    if idx1 and options_meta[idx1] then
      action = options_meta[idx1].action
      on_select_fn = options_meta[idx1].on_select
    end
    local inputs = collect_inputs(input_leaves)
    if type(on_select_fn) == "function" then
      local ok, err = pcall(on_select_fn)
      if not ok then
        smelt.notify_error("dialog on_select: " .. tostring(err))
      end
    end
    smelt.win.close(win_id)
    smelt.task.resume(task_id, {
      action = action,
      option_index = idx1,
      inputs = inputs,
    })
  end)

  smelt.win.on_event(win_id, "dismiss", function()
    smelt.win.close(win_id)
    smelt.task.resume(task_id, {
      action = "dismiss",
      inputs = collect_inputs(input_leaves),
    })
  end)

  if type(opts.keymaps) == "table" then
    for _, km in ipairs(opts.keymaps) do
      if type(km) == "table" and km.key and type(km.on_press) == "function" then
        local on_press = km.on_press
        smelt.win.set_keymap(win_id, km.key, function(raw_ctx)
          local ctx = build_ctx(raw_ctx, win_id, task_id, nil, input_leaves)
          local ok, err = pcall(on_press, ctx)
          if not ok then
            smelt.notify_error("dialog keymap: " .. tostring(err))
          end
        end)
      end
    end
  end

  if next(input_on_change) ~= nil then
    smelt.win.on_event(win_id, "text_changed", function(raw_ctx)
      local ctx = build_ctx(raw_ctx, win_id, task_id, nil, input_leaves)
      for _, fn in pairs(input_on_change) do
        local ok, err = pcall(fn, ctx)
        if not ok then
          smelt.notify_error("dialog on_change: " .. tostring(err))
        end
      end
    end)
  end

  if type(opts.on_tick) == "function" then
    local on_tick = opts.on_tick
    smelt.win.on_event(win_id, "tick", function(raw_ctx)
      local ctx = build_ctx(raw_ctx, win_id, task_id, nil, input_leaves)
      local ok, err = pcall(on_tick, ctx)
      if not ok then
        smelt.notify_error("dialog on_tick: " .. tostring(err))
      end
    end)
  end

  return smelt.task.wait(task_id)
end

return M
