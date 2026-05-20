-- `smelt.dialog`: opinionated framing on top of `smelt.overlay`.
--
-- A dialog is always docked at the bottom of the screen with a top border and is modal.
-- For anything else (centered info viewers, transient overlays), use `smelt.overlay`
-- directly.
--
-- The dialog primitive does not know what's inside it. Consumers build their own
-- buffers and leaves (using the helpers below or `smelt.win.new` directly) and pass
-- the leaves in `opts.panels`. Dialog handles only:
--   1. Opening the overlay at dock_bottom with a top border.
--   2. Setting initial focus.
--   3. Installing dialog-level keymaps at overlay scope.
--   4. Bridging Submit/Dismiss/Tick events to user callbacks.
--   5. Coroutine lifecycle: `open(opts)` blocks until `ctx.resolve(v)` is called.
--
-- Keymap scoping (important):
--   - `opts.keymaps` are DIALOG-WIDE — installed at overlay scope (tier 1b of
--     the key cascade) so they fire regardless of which panel is focused.
--     Use these for shortcuts that should always work in the dialog (e.g.
--     Alt-W, Ctrl-D).
--   - To scope a key to a specific panel, install it directly on that leaf via
--     `leaf:key(key, fn)` after `open_handle` returns. Example:
--     the confirm dialog binds `tab` only on the options leaf (jump into the
--     reason input) and `esc` only on the reason leaf (pop focus back to the
--     options leaf instead of dismissing the dialog).
--
-- Buffer helpers:
--   smelt.dialog.input(placeholder)         -> leaf, buf  (single-line input)
--   smelt.dialog.options(labels, opts)      -> leaf, buf  (list of selectable labels)
--   smelt.dialog.list(buf, opts)            -> leaf       (existing buffer as a list)
--   smelt.dialog.markdown(text)             -> leaf, buf  (markdown-rendered content)
--   smelt.dialog.content(opts)              -> leaf, buf  (plain content; opts.text or opts.buf)

local M = {}

smelt.dialog = smelt.dialog or {}

local REGION = "dialog_overlay"

-- ── Buffer/leaf builders ──────────────────────────────────────────────
--
-- Every helper here adds a one-cell gutter on the left AND the right so dialog
-- content never sits flush against the frame. The gutter is invariant: callers
-- must not pass `pad_left` / `pad_right`. Custom leaves built outside these
-- helpers and handed to `smelt.dialog.open` must follow the same rule.
--
-- Scrollbars: buffer-viewer leaves (`markdown`, `content`) inherit the default
-- `scrollbar = true` from `smelt.win.new` so a thumb appears when content
-- overflows. Cursor-driven leaves (`input`, `options`, `list`) opt out — the
-- selection cursor and key nav already convey position.

local GUTTER = 1

function smelt.dialog.input(placeholder, opts)
  opts = opts or {}
  local buf = smelt.buf.new()
  buf:lines({ "" })
  -- Single-line input: wrap=false keeps long entries on one row so the caret
  -- can scroll horizontally instead of jumping to a wrapped continuation.
  -- `opts.pad_left` / `opts.pad_right` override the dialog gutter for callers
  -- that want extra indent (e.g. nested inputs visually grouped under a list).
  local leaf = smelt.win.new(buf, {
    region = REGION, focusable = true, selectable = true,
    pad_left = opts.pad_left or GUTTER,
    pad_right = opts.pad_right or GUTTER,
    scrollbar = false, wrap = false,
    kind = "input",
    placeholder = placeholder or "",
  })
  return leaf, buf
end

function smelt.dialog.options(labels, opts)
  opts = opts or {}
  local lines = {}
  for _, l in ipairs(labels or {}) do table.insert(lines, l) end
  if #lines == 0 then lines = { "" } end
  local buf = smelt.buf.new()
  buf:lines(lines)
  local selected = tonumber(opts.selected or 1) or 1
  if selected < 1 then selected = 1 end
  local leaf = smelt.win.new(buf, {
    region        = REGION,
    focusable     = true,
    selectable    = true,
    pad_left      = GUTTER,
    pad_right     = GUTTER,
    scrollbar     = false,
    kind          = "list",
    initial_cursor = selected - 1,
  })
  return leaf, buf
end

function smelt.dialog.list(buf, opts)
  opts = opts or {}
  local focusable = opts.focusable
  if focusable == nil then focusable = true end
  local leaf = smelt.win.new(buf, {
    region = REGION, focusable = focusable, selectable = true,
    pad_left = GUTTER, pad_right = GUTTER, scrollbar = false,
    kind = "list",
    initial_cursor = opts.selected or 0,
  })
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

function smelt.dialog.markdown(text)
  local buf = smelt.buf.new({ mode = "markdown" })
  buf:source(text or "")
  local leaf = smelt.win.new(buf, {
    region = REGION, focusable = false, selectable = true,
    pad_left = GUTTER, pad_right = GUTTER,
  })
  return leaf, buf
end

function smelt.dialog.content(opts)
  opts = opts or {}
  local buf = opts.buf
  if not buf then
    buf = smelt.buf.new({ readonly = true })
    if opts.text and opts.text ~= "" then
      buf:lines(split_lines(opts.text))
    end
  end
  local focusable = opts.focusable
  if focusable == nil then focusable = opts.interactive or false end
  -- `wrap` defaults to true (matches `smelt.win.new`); pass `wrap = false` to
  -- show pre-styled content (e.g. via `buf:styled(...)`) at its
  -- intrinsic width without soft-wrapping the row.
  local leaf = smelt.win.new(buf, {
    region      = REGION,
    focusable   = focusable,
    selectable  = true,
    vim_enabled = (opts.interactive and smelt.settings.vim) and true or false,
    pad_left    = GUTTER,
    pad_right   = GUTTER,
    wrap        = opts.wrap,
  })
  return leaf, buf
end

-- ── Dialog overlay wrapper ────────────────────────────────────────────

-- Build the overlay items table from panels and open the overlay. Returns
-- the root leaf and the array of leaves.
--
-- Dialog height (pick one; setting both is an error):
--   * `opts.height`     — fixed size: integer cells, `"N%"`, `"fill"`. Default `"60%"`.
--   * `opts.max_height` — shrink to content, capped at this size.
--   * `opts.min_height` — floor that pairs with either mode. Fit-mode dialogs
--                         default to `min_height = "30%"` so a placeholder
--                         body stays visible when content collapses; pass
--                         `min_height = 0` to opt out.
--
-- All three knobs are **body-relative** when given as integer cells: the
-- wrapper adds the dialog's chrome (top border + title row, 1 cell) before
-- forwarding to the overlay (which uses total-rect semantics). `"N%"` /
-- `"fill"` / `"fit"` are forwarded verbatim — percentages of the terminal
-- don't compose with absolute chrome offsets, and the extra row is negligible
-- at typical percentages anyway.

-- The dialog draws a single chrome row at the top (border + title share it).
local CHROME_H = 1

-- Convert a body-relative size spec to a total-overlay spec. Integer cells
-- get the chrome offset added; non-numeric specs (`"N%"`, `"fill"`, `"fit"`)
-- pass through unchanged.
local function with_chrome(spec)
  if type(spec) == "number" then return spec + CHROME_H end
  return spec
end

local function open_overlay(opts)
  if opts.height ~= nil and opts.max_height ~= nil then
    error("smelt.dialog: use `height` (fixed) or `max_height` (fit to content), not both", 3)
  end
  local fit_mode = opts.max_height ~= nil
  local default_panel_height = fit_mode and "fit" or nil
  -- Fit-mode dialogs read their natural size — for trivial content (one
  -- placeholder line) that collapses to just the chrome row. Default to a
  -- 30% terminal-height floor so the placeholder + a comfortable margin stay
  -- visible. Callers can override via `opts.min_height` (including `0` to opt
  -- out entirely).
  local default_min_height = fit_mode and "30%" or nil

  local panels = opts.panels or {}
  if #panels == 0 then
    error("smelt.dialog: panels must be non-empty", 3)
  end

  local leaves = {}
  local layout_items = {}
  for i, p in ipairs(panels) do
    if type(p) ~= "table" or p.leaf == nil then
      error("smelt.dialog: panel " .. i .. " requires a `leaf`", 3)
    end
    leaves[i] = p.leaf
    local leaf_node = smelt.overlay.layout.leaf(p.leaf, {
      border              = p.border,
      title               = p.title,
      collapse_when_empty = p.collapse_when_empty or false,
    })
    layout_items[i] = { leaf_node, height = p.height or default_panel_height }
  end

  -- The wrapper is responsible for the single-cell gutter on each side of the
  -- title content. Callers MUST NOT pad — pass `"messages"` not `" messages "`,
  -- and for multi-span titles drop the leading space on the first span and the
  -- trailing space on the last span.
  --   - bare string: rendered dim and padded with a space on each side.
  --   - table with `text` key (single span): wrapped between two raw space spans.
  --   - table sequence (multi-span): same — leading/trailing space spans added.
  local title = opts.title
  if type(title) == "string" and title ~= "" then
    title = { { text = " " }, { text = title, dim = true }, { text = " " } }
  elseif type(title) == "table" then
    if title.text ~= nil then
      title = { { text = " " }, title, { text = " " } }
    elseif #title > 0 then
      local padded = { { text = " " } }
      for _, span in ipairs(title) do table.insert(padded, span) end
      table.insert(padded, { text = " " })
      title = padded
    end
  end

  local panel_vbox = smelt.overlay.layout.vbox(layout_items)

  -- fixed: width = "100%", height = opts.height (or "60%" default)
  -- fit:   width = "100%", height = "fit", max_height = opts.max_height
  local height_spec, max_height_spec
  if opts.max_height ~= nil then
    height_spec, max_height_spec = "fit", opts.max_height
  else
    height_spec, max_height_spec = (opts.height or "60%"), nil
  end

  local overlay = smelt.overlay.new({
    title        = title,
    anchor       = "dock_bottom",
    border       = { top = "SmeltAccent" },
    modal        = true,
    blocks_agent = opts.blocks_agent or false,
    layout       = panel_vbox,
    width        = "100%",
    height       = with_chrome(height_spec),
    max_height   = with_chrome(max_height_spec),
    min_height   = with_chrome(opts.min_height or default_min_height),
  })

  return leaves[1], leaves, overlay
end

-- Wire dialog-level keymaps, focus, events, and the resolve handle. Shared between
-- `open` (coroutine) and `open_handle` (sync).
local function setup_lifecycle(opts, leaves, overlay, resolve_fn)
  local root = leaves[1]

  -- Explicit focus override; otherwise the overlay's own modal-focus logic picks
  -- the first focusable leaf at open() time.
  if opts.focus then opts.focus:focus() end

  local resolved = false
  local function resolve(value)
    if resolved then return end
    resolved = true
    root:close()
    resolve_fn(value)
  end

  -- Build a ctx for user callbacks. Raw event fields (`text`, `index`, `code`,
  -- `mods`, `leaf`) flow through unchanged; we add `win` (the dialog root),
  -- `resolve`, `close`, and `panels` on top.
  local function make_ctx(raw_ctx)
    local ctx = {}
    if type(raw_ctx) == "table" then
      for k, v in pairs(raw_ctx) do ctx[k] = v end
    end
    ctx.win     = root
    ctx.resolve = resolve
    ctx.close   = function() resolve(nil) end
    ctx.panels  = leaves
    return ctx
  end

  -- Dialog-level keymaps install at the overlay scope so they fire regardless
  -- of which leaf holds focus, without per-leaf re-registration. Tier 1b of
  -- the key cascade routes the chord to whichever overlay contains the focused
  -- leaf, which is always this overlay while the dialog is open + modal.
  if type(opts.keymaps) == "table" then
    for _, km in ipairs(opts.keymaps) do
      if type(km) == "table" and km.key and type(km.on_press) == "function" then
        local on_press = km.on_press
        overlay:key(km.key, function(raw_ctx)
          local ok, err = pcall(on_press, make_ctx(raw_ctx))
          if not ok then smelt.notify.error("dialog keymap: " .. tostring(err)) end
        end)
      end
    end
  end

  -- Events fire on the leaf that emits them (no implicit bubbling), so dialog-wide
  -- handlers register on every leaf to catch events from any panel.
  local function register_on_all(event_name, handler)
    for _, leaf in ipairs(leaves) do
      leaf:on(event_name, handler)
    end
  end

  -- Submit: fires `opts.on_submit` if provided. With no handler, the dialog stays
  -- open — Enter doing nothing is easier to diagnose than Enter mysteriously
  -- closing. Esc/Ctrl-C still dismisses via the Dismiss event below.
  if type(opts.on_submit) == "function" then
    register_on_all("submit", function(raw_ctx)
      local ok, err = pcall(opts.on_submit, make_ctx(raw_ctx))
      if not ok then smelt.notify.error("dialog on_submit: " .. tostring(err)) end
    end)
  end

  -- Dismiss: Esc / Ctrl-C / outside-modal click. Defaults to resolve(nil).
  register_on_all("dismiss", function(raw_ctx)
    if type(opts.on_dismiss) == "function" then
      local ok, err = pcall(opts.on_dismiss, make_ctx(raw_ctx))
      if not ok then smelt.notify.error("dialog on_dismiss: " .. tostring(err)) end
    else
      resolve(nil)
    end
  end)

  if type(opts.on_tick) == "function" then
    register_on_all("tick", function(raw_ctx)
      local ok, err = pcall(opts.on_tick, make_ctx(raw_ctx))
      if not ok then smelt.notify.error("dialog on_tick: " .. tostring(err)) end
    end)
  end

  if type(opts.on_event) == "table" then
    for event_name, fn in pairs(opts.on_event) do
      if type(fn) == "function" then
        register_on_all(event_name, function(raw_ctx)
          local ok, err = pcall(fn, make_ctx(raw_ctx))
          if not ok then smelt.notify.error("dialog on_event[" .. event_name .. "]: " .. tostring(err)) end
        end)
      end
    end
  end

  return resolve, root
end

-- Coroutine-blocking open. Returns the value passed to `ctx.resolve(value)`.
function smelt.dialog.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.dialog.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.dialog.open: expected table of options", 2)
  end

  local _, leaves, overlay = open_overlay(opts)
  local task_id = smelt.task.alloc()
  setup_lifecycle(opts, leaves, overlay, function(value)
    smelt.task.resume(task_id, value)
  end)
  return smelt.task.wait(task_id)
end

-- ── Picker preset ─────────────────────────────────────────────────────
--
-- `smelt.dialog.picker(opts)` is a thin wrapper that bundles the recurring
-- Telescope-style shape: a single-line input on top, a non-focusable list
-- below, navigation forwarded from the input to the list, Enter submits.
-- Coroutine-blocking like `smelt.dialog.open`; returns the value resolved
-- from `on_submit` (or `nil` on dismiss).
--
-- Opts:
--   * `items`       — array of arbitrary item tables (passed to `render`).
--   * `render`      — `function(item) -> { text = ..., marks = ... }` (see
--                     `smelt.list`).
--   * `filter`      — optional predicate `function(item) -> bool` applied
--                     to every refilter; the picker re-runs it whenever
--                     the query changes (so it can close over the live
--                     query state).
--   * `placeholder` — input prompt text. Defaults to `""`.
--   * `empty_text`  — shown in the list when nothing matches.
--   * `on_open`     — `function(ctx)` fires once before the dialog blocks,
--                     after the input/list have been built. Use it to seed
--                     marks on the input buffer or to set an initial cursor
--                     row on the list.
--   * `on_query`    — `function(query, ctx)` fires on every keystroke.
--                     The default is `list:set_filter(opts.filter)`. Pass
--                     this when you want to swap the filter (e.g. rebuild
--                     it from a fresh query).
--   * `on_submit`   — `function(item, ctx)` fires on Enter. Default:
--                     `ctx.resolve(item)`. Override when you need to
--                     post-process before resolving (or to no-op when
--                     nothing is selected).
--   * `keymaps`     — extra dialog-level keymaps merged on top of the
--                     built-in navigation bindings. Each entry's
--                     `on_press(ctx)` receives the picker ctx with
--                     `ctx.list`, `ctx.input`, `ctx.input_buf` added.
--   * `title`, `height`, `max_height`, `min_height`, `blocks_agent` — forwarded to
--                     `smelt.dialog.open`.

local NAV_KEYS = {
  { "up",     -1  },
  { "down",   1   },
  { "ctrl-k", -1  },
  { "ctrl-j", 1   },
  { "ctrl-p", -1  },
  { "ctrl-n", 1   },
  { "pgup",   -10 },
  { "pgdn",   10  },
  { "ctrl-u", -5  },
  { "ctrl-d", 5   },
}

function smelt.dialog.picker(opts)
  if not coroutine.isyieldable() then
    error("smelt.dialog.picker: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.dialog.picker: expected table of options", 2)
  end
  if type(opts.render) ~= "function" then
    error("smelt.dialog.picker: opts.render must be a function", 2)
  end

  local input_leaf, input_buf = smelt.dialog.input(opts.placeholder or "")
  local list_buf  = smelt.buf.new()
  local list_leaf = smelt.dialog.list(list_buf, { focusable = false })

  local list = smelt.list.new({
    leaf       = list_leaf,
    buf        = list_buf,
    items      = opts.items or {},
    render     = opts.render,
    filter     = opts.filter,
    empty_text = opts.empty_text or "  (no matches)",
  })

  local function augment(ctx)
    ctx.list      = list
    ctx.input     = input_leaf
    ctx.input_buf = input_buf
    return ctx
  end

  if type(opts.on_open) == "function" then
    opts.on_open(augment({}))
  end

  input_leaf:on("text_changed", function(raw)
    local query = (raw and raw.text) or ""
    if type(opts.on_query) == "function" then
      opts.on_query(query, augment({ text = query }))
    elseif opts.filter ~= nil then
      list:set_filter(opts.filter)
    end
  end)

  local function nav(delta)
    return function() list:move_cursor(delta) end
  end

  local keymaps = {}
  for _, n in ipairs(NAV_KEYS) do
    table.insert(keymaps, { key = n[1], on_press = nav(n[2]) })
  end
  if type(opts.keymaps) == "table" then
    for _, km in ipairs(opts.keymaps) do
      local fn = km.on_press
      table.insert(keymaps, {
        key      = km.key,
        hint     = km.hint,
        on_press = function(ctx) return fn(augment(ctx)) end,
      })
    end
  end

  local on_submit
  if type(opts.on_submit) == "function" then
    on_submit = function(ctx) return opts.on_submit(list:selected(), augment(ctx)) end
  else
    on_submit = function(ctx)
      local item = list:selected()
      if item ~= nil then ctx.resolve(item) end
    end
  end

  return smelt.dialog.open({
    title        = opts.title,
    height       = opts.height,
    max_height   = opts.max_height,
    min_height   = opts.min_height,
    blocks_agent = opts.blocks_agent,
    panels = {
      { leaf = input_leaf, height = 1      },
      { leaf = list_leaf,  height = "fill" },
    },
    focus     = input_leaf,
    keymaps   = keymaps,
    on_submit = on_submit,
    on_dismiss = opts.on_dismiss,
  })
end

-- Non-coroutine open. Returns `{ win, panels, close() }` synchronously. The consumer
-- drives the lifecycle via `on_submit` / `on_dismiss` callbacks and tears down with
-- `handle:close()`. No value flows back (use `open` if you want one).
function smelt.dialog.open_handle(opts)
  if type(opts) ~= "table" then
    error("smelt.dialog.open_handle: expected table of options", 2)
  end
  local _, leaves, overlay = open_overlay(opts)
  local resolve, root = setup_lifecycle(opts, leaves, overlay, function(_) end)
  return {
    win    = root,
    panels = leaves,
    close  = function() resolve(nil) end,
  }
end

return M
