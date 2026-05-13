-- `smelt.ui.dialog`: opinionated framing on top of `smelt.ui.overlay`.
--
-- A dialog is always docked at the bottom of the screen with a top border and is modal.
-- For anything else (centered info viewers, transient overlays), use `smelt.ui.overlay`
-- directly.
--
-- The dialog primitive does not know what's inside it. Consumers build their own
-- buffers and leaves (using the helpers below or `smelt.win.open` directly) and pass
-- the leaves in `opts.panels`. Dialog handles only:
--   1. Opening the overlay at dock_bottom with a top border.
--   2. Setting initial focus.
--   3. Installing dialog-level keymaps on every leaf.
--   4. Bridging Submit/Dismiss/Tick events to user callbacks.
--   5. Coroutine lifecycle: `open(opts)` blocks until `ctx.resolve(v)` is called.
--
-- Keymap scoping (important):
--   - `opts.keymaps` are DIALOG-WIDE — installed on every leaf so they fire regardless
--     of which panel is focused. Use these for shortcuts that should always work in
--     the dialog (e.g. Alt-W, Ctrl-D).
--   - To scope a key to a specific panel, install it directly on that leaf via
--     `smelt.win.set_keymap(leaf, key, fn)` after `open_handle` returns. Example:
--     the confirm dialog binds `e` only on the options leaf so that typing `e` while
--     editing the reason input still types a literal `e`.
--
-- Buffer helpers:
--   smelt.ui.dialog.input(placeholder)         -> leaf, buf  (single-line input)
--   smelt.ui.dialog.options(labels, opts)      -> leaf, buf  (list of selectable labels)
--   smelt.ui.dialog.list(buf, opts)            -> leaf       (existing buffer as a list)
--   smelt.ui.dialog.markdown(text)             -> leaf, buf  (markdown-rendered content)
--   smelt.ui.dialog.content(opts)              -> leaf, buf  (plain content; opts.text or opts.buf)

local M = {}

smelt.ui.dialog = smelt.ui.dialog or {}

local REGION = "dialog_overlay"

-- ── Buffer/leaf builders ──────────────────────────────────────────────

function smelt.ui.dialog.input(placeholder)
  local buf = smelt.buf.create()
  smelt.buf.set_lines(buf, { "" })
  -- One-cell left gutter so the cursor and placeholder don't sit flush against
  -- the dialog frame.
  local leaf = smelt.win.open(buf, { region = REGION, focusable = true, pad_left = 1 })
  if leaf then smelt.win.configure_input(leaf, placeholder or "") end
  return leaf, buf
end

function smelt.ui.dialog.options(labels, opts)
  opts = opts or {}
  local lines = {}
  for _, l in ipairs(labels or {}) do table.insert(lines, l) end
  if #lines == 0 then lines = { "" } end
  local buf = smelt.buf.create()
  smelt.buf.set_lines(buf, lines)
  local leaf = smelt.win.open(buf, {
    region    = REGION,
    focusable = true,
    pad_left  = opts.pad_left,
    pad_right = opts.pad_right,
  })
  if leaf then
    local selected = tonumber(opts.selected or 1) or 1
    if selected < 1 then selected = 1 end
    smelt.win.configure_list(leaf, selected - 1)
  end
  return leaf, buf
end

function smelt.ui.dialog.list(buf, opts)
  opts = opts or {}
  local focusable = opts.focusable
  if focusable == nil then focusable = true end
  local leaf = smelt.win.open(buf, { region = REGION, focusable = focusable })
  if leaf then smelt.win.configure_list(leaf, opts.selected or 0) end
  return leaf
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

function smelt.ui.dialog.markdown(text)
  local buf = smelt.buf.create({ mode = "markdown" })
  smelt.buf.set_source(buf, text or "")
  local leaf = smelt.win.open(buf, { region = REGION, focusable = false })
  return leaf, buf
end

function smelt.ui.dialog.content(opts)
  opts = opts or {}
  local buf = opts.buf
  if not buf then
    buf = smelt.buf.create()
    if opts.text and opts.text ~= "" then
      smelt.buf.set_lines(buf, split_lines(opts.text))
    end
  end
  local focusable = opts.focusable
  if focusable == nil then focusable = opts.interactive or false end
  local leaf = smelt.win.open(buf, {
    region = REGION,
    focusable = focusable,
    vim_enabled = opts.interactive or false,
  })
  return leaf, buf
end

-- ── Dialog overlay wrapper ────────────────────────────────────────────

-- Build the overlay items table from panels and open the overlay. Returns the root
-- leaf and the array of leaves.
local function open_overlay(opts)
  local panels = opts.panels or {}
  if #panels == 0 then
    error("smelt.ui.dialog: panels must be non-empty", 3)
  end

  local leaves = {}
  local overlay_items = {}
  for i, p in ipairs(panels) do
    if type(p) ~= "table" or p.leaf == nil then
      error("smelt.ui.dialog: panel " .. i .. " requires a `leaf`", 3)
    end
    leaves[i] = p.leaf
    overlay_items[i] = {
      win                 = p.leaf,
      height              = p.height,
      collapse_when_empty = p.collapse_when_empty or false,
      border              = p.border,
      title               = p.title,
    }
  end

  -- Plain string titles get padded + dimmed so they read as " title " against
  -- the top border. Pass a table title to opt out (e.g. `{ text = "foo", bold = true }`).
  local title = opts.title
  if type(title) == "string" and title ~= "" then
    title = { text = " " .. title .. " ", dim = true }
  end

  smelt.ui.overlay.open({
    title            = title,
    placement        = "dock_bottom",
    placement_height = opts.height or 60,
    border           = { top = "SmeltAccent" },
    modal            = true,
    blocks_agent     = opts.blocks_agent or false,
    items            = overlay_items,
  })

  return leaves[1], leaves
end

-- Wire dialog-level keymaps, focus, events, and the resolve handle. Shared between
-- `open` (coroutine) and `open_handle` (sync).
local function setup_lifecycle(opts, leaves, resolve_fn)
  local root = leaves[1]

  -- Explicit focus override; otherwise the overlay's own modal-focus logic picks
  -- the first focusable leaf at open() time.
  if opts.focus then smelt.win.set_focus(opts.focus) end

  local resolved = false
  local function resolve(value)
    if resolved then return end
    resolved = true
    smelt.win.close(root)
    resolve_fn(value)
  end

  -- Build a ctx for user callbacks. Raw event fields (`text`, `index`, `code`, `mods`,
  -- `win`) flow through unchanged; we add `resolve`, `close`, and `panels` on top.
  local function make_ctx(raw_ctx)
    local ctx = {}
    if type(raw_ctx) == "table" then
      for k, v in pairs(raw_ctx) do ctx[k] = v end
    end
    if not ctx.win then ctx.win = root end
    ctx.resolve = resolve
    ctx.close   = function() resolve(nil) end
    ctx.panels  = leaves
    return ctx
  end

  -- Dialog-level keymaps: install on EVERY leaf so they fire no matter which panel
  -- has focus. Inert leaves never receive keys, so installing widely is harmless.
  if type(opts.keymaps) == "table" then
    for _, km in ipairs(opts.keymaps) do
      if type(km) == "table" and km.key and type(km.on_press) == "function" then
        local on_press = km.on_press
        local cb = function(raw_ctx)
          local ok, err = pcall(on_press, make_ctx(raw_ctx))
          if not ok then smelt.ui.notify_error("dialog keymap: " .. tostring(err)) end
        end
        for _, leaf in ipairs(leaves) do
          smelt.win.set_keymap(leaf, km.key, cb)
        end
      end
    end
  end

  -- Events fire on the leaf that emits them (no implicit bubbling), so dialog-wide
  -- handlers register on every leaf to catch events from any panel.
  local function register_on_all(event_name, handler)
    for _, leaf in ipairs(leaves) do
      smelt.win.on_event(leaf, event_name, handler)
    end
  end

  -- Submit: fires `opts.on_submit` if provided. With no handler, the dialog stays
  -- open — Enter doing nothing is easier to diagnose than Enter mysteriously
  -- closing. Esc/Ctrl-C still dismisses via the Dismiss event below.
  if type(opts.on_submit) == "function" then
    register_on_all("submit", function(raw_ctx)
      local ok, err = pcall(opts.on_submit, make_ctx(raw_ctx))
      if not ok then smelt.ui.notify_error("dialog on_submit: " .. tostring(err)) end
    end)
  end

  -- Dismiss: Esc / Ctrl-C / outside-modal click. Defaults to resolve(nil).
  register_on_all("dismiss", function(raw_ctx)
    if type(opts.on_dismiss) == "function" then
      local ok, err = pcall(opts.on_dismiss, make_ctx(raw_ctx))
      if not ok then smelt.ui.notify_error("dialog on_dismiss: " .. tostring(err)) end
    else
      resolve(nil)
    end
  end)

  if type(opts.on_tick) == "function" then
    register_on_all("tick", function(raw_ctx)
      local ok, err = pcall(opts.on_tick, make_ctx(raw_ctx))
      if not ok then smelt.ui.notify_error("dialog on_tick: " .. tostring(err)) end
    end)
  end

  if type(opts.on_event) == "table" then
    for event_name, fn in pairs(opts.on_event) do
      if type(fn) == "function" then
        register_on_all(event_name, function(raw_ctx)
          local ok, err = pcall(fn, make_ctx(raw_ctx))
          if not ok then smelt.ui.notify_error("dialog on_event[" .. event_name .. "]: " .. tostring(err)) end
        end)
      end
    end
  end

  return resolve, root
end

-- Coroutine-blocking open. Returns the value passed to `ctx.resolve(value)`.
function smelt.ui.dialog.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.ui.dialog.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.ui.dialog.open: expected table of options", 2)
  end

  local _, leaves = open_overlay(opts)
  local task_id = smelt.task.alloc()
  setup_lifecycle(opts, leaves, function(value)
    smelt.task.resume(task_id, value)
  end)
  return smelt.task.wait(task_id)
end

-- Non-coroutine open. Returns `{ win, panels, close() }` synchronously. The consumer
-- drives the lifecycle via `on_submit` / `on_dismiss` callbacks and tears down with
-- `handle:close()`. No value flows back (use `open` if you want one).
function smelt.ui.dialog.open_handle(opts)
  if type(opts) ~= "table" then
    error("smelt.ui.dialog.open_handle: expected table of options", 2)
  end
  local _, leaves = open_overlay(opts)
  local resolve, root = setup_lifecycle(opts, leaves, function(_) end)
  return {
    win    = root,
    panels = leaves,
    close  = function() resolve(nil) end,
  }
end

return M
