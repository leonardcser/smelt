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
--   - `opts.keymaps` are DIALOG-WIDE - installed at overlay scope (tier 1b of
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
--   smelt.dialog.input(placeholder)         -> leaf, buf, input  (single-line input)
--   smelt.dialog.menu(items, opts)          -> leaf, ctrl (numbered selectable list)
--   smelt.dialog.list(buf, opts)            -> leaf       (existing buffer as a list)
--   smelt.dialog.markdown(text)             -> leaf, buf  (markdown-rendered content)
--   smelt.dialog.content(opts)              -> leaf, buf  (plain content; opts.text or opts.buf)
--   smelt.dialog.viewer(opts)               -> handle, buf, leaf  (read-only content dialog)
--
-- Dialog context (active dialog introspection):
--   smelt.dialog.current()                  -> ctx | nil  (resolve/close/panels/focused_leaf)
--
-- Callable from any handler running while the dialog is open - including
-- leaf-level `leaf:key(...)` callbacks, which normally don't see the
-- dialog's resolve handle. Nested dialogs stack; `current()` always
-- returns the topmost.

local M = {}

smelt.dialog = smelt.dialog or {}

local REGION = "dialog_overlay"

local function report_callback_error(event, err)
  local msg = tostring(err)
  smelt.log.error("dialog.callback_failed", { event = event, error = msg })
  smelt.notify.error("dialog " .. event .. ": " .. msg)
end

-- Stack of active dialog contexts. Pushed by `setup_lifecycle` at open
-- time, popped on resolve. `smelt.dialog.current()` returns the topmost
-- ctx so nested dialogs (e.g. confirm-on-top-of-picker) don't shadow each
-- other.
local dialog_stack = {}

--- Return the topmost active dialog ctx (the same shape passed to
--- `on_submit`/`keymap` handlers: `{ resolve, close, win, panels,
--- focused_leaf }`), or `nil` if no dialog is open. Use it inside
--- `leaf:key(...)` callbacks - those normally lack a path to the
--- dialog's resolve handle.
---@type fun(): table | nil
function smelt.dialog.current()
  return dialog_stack[#dialog_stack]
end

-- ── Buffer/leaf builders ──────────────────────────────────────────────
--
-- Every helper here adds a one-cell gutter on the left AND the right so dialog
-- content never sits flush against the frame. The gutter is invariant: callers
-- must not pass `pad_left` / `pad_right`. Custom leaves built outside these
-- helpers and handed to `smelt.dialog.open` must follow the same rule.
--
-- Scrollbars: buffer-viewer leaves (`markdown`, `content`) inherit the default
-- `scrollbar = true` from `smelt.win.new` so a thumb appears when content
-- overflows. Cursor-driven leaves (`input`, `options`, `list`) opt out - the
-- selection cursor and key nav already convey position.

local GUTTER = 1

--- One body panel inside a dialog. `leaf` is the win/leaf built by one of
--- the `smelt.dialog.*` helpers; `height` follows the same grammar as
--- `smelt.dialog.open` (integer cells, `"N%"`, `"fill"`, `"fit"`).
---@class smelt.dialog.Panel
---@field leaf smelt.win.Win A leaf returned by `smelt.dialog.input/options/list/markdown/content`.
---@field height? any Integer cells, `"N%"`, `"fill"`, or `"fit"`.

--- One dialog-level keymap entry. `on_press(ctx)` receives the dialog
--- context exposing `ctx.close()` and `ctx.resolve(value)` so the
--- handler can dismiss the dialog or resolve the blocking `open` call.
---@class smelt.dialog.Keymap
---@field key string Chord string (e.g. `"q"`, `"<Esc>"`, `"ctrl-j"`).
---@field hint? string Optional one-line hint surfaced in the dialog footer.
---@field on_press fun(ctx: any): any Handler invoked when the key fires.

--- Options accepted by `smelt.dialog.open` / `smelt.dialog.open_handle`.
--- Body sizing is body-relative: integer `height` values are forwarded
--- through with the chrome row added automatically; `"N%"`, `"fill"`,
--- and `"fit"` pass through verbatim. Pick one of `height` or
--- `max_height`; setting both raises.
---@class smelt.dialog.Opts
---@field title? string Title rendered in the chrome row.
---@field panels smelt.dialog.Panel[] Ordered list of body panels.
---@field bottom_panels? smelt.dialog.Panel[] Panels pinned to the bottom when the dialog has surplus height; extra height is placed between them and `panels`.
---@field bottom_gap? integer Minimum blank rows between `panels` and `bottom_panels` (default 0).
---@field focus? smelt.win.Win Leaf that should receive initial focus.
---@field height? any Fixed total body size: integer cells, `"N%"`, `"fill"`, or `"fit"`.
---@field max_height? any Shrink-to-content cap that pairs with `min_height`.
---@field min_height? any Floor for the body size (defaults to `"30%"` in fit mode).
---@field blocks_agent? boolean Block the agent loop while the dialog is open. Defaults to `false`.
---@field border? table Top border style override; defaults to `{ top = "SmeltAccent" }`.
---@field resizable? boolean Set `false` to disable the default top-edge resize handle.
---@field keymaps? smelt.dialog.Keymap[] Dialog-level key bindings (merged with built-ins).
---@field on_submit? fun(ctx: any): any Handler invoked on Enter; default resolves with the focused leaf.
---@field on_dismiss? fun(): nil Handler invoked when the dialog is dismissed.
---@field on_close? fun(ctx: any): nil Handler invoked once whenever the dialog resolves or closes.

--- Options accepted by `smelt.dialog.picker`. Layered on top of
--- `smelt.dialog.Opts`; only the picker-specific fields are listed.
---@class smelt.dialog.PickerOpts
---@field items? any[] | fun(): any[] Eager item table or a lazy producer; re-evaluated by `on_query`.
---@field render fun(item: any): table Per-item `{ text, marks }` table - see `smelt.list.new`.
---@field filter? fun(item: any): boolean Predicate applied during `set_filter` / `refresh`.
---@field placeholder? string Input placeholder; defaults to `""`.
---@field empty_text? string Shown in the list when nothing matches.
---@field on_open? fun(ctx: any): nil Fires once after the input/list have been built.
---@field on_query? fun(query: string, ctx: any): nil Fires on every keystroke; default re-applies `filter`.
---@field on_submit? fun(ctx: any): any Fires on Enter. `ctx.item` is the highlighted row; defaults to resolving with `ctx.item` when non-nil.
---@field on_dismiss? fun(): nil Fires when the dialog is dismissed.
---@field keymaps? smelt.dialog.Keymap[] Extra dialog-level keymaps merged on top of navigation bindings.
---@field title? string Forwarded to `smelt.dialog.open`.
---@field height? any Forwarded to `smelt.dialog.open`.
---@field max_height? any Forwarded to `smelt.dialog.open`.
---@field min_height? any Forwarded to `smelt.dialog.open`.
---@field blocks_agent? boolean Forwarded to `smelt.dialog.open`.

-- Build a single-line text-input leaf with a fresh buffer. `placeholder`
-- shows when the buffer is empty; `opts.pad_left` / `opts.pad_right`
-- override the dialog gutter. Returns `(leaf, buf, input)` so callers can
-- keep using the buffer directly or opt into the first-class input handle.
---@type fun(placeholder: string?, opts: table?): smelt.win.Win, smelt.buf.Buf, smelt.input.Input
function smelt.dialog.input(placeholder, opts)
  opts = opts or {}
  local input = smelt.input.new({
    region = REGION,
    placeholder = placeholder or "",
    pad_left = opts.pad_left or GUTTER,
    pad_right = opts.pad_right or GUTTER,
    scrollbar = false,
    wrap = false,
  })
  return input:win(), input:buf(), input
end

-- ── Menu primitive ─────────────────────────────────────────────────────
--
-- `smelt.dialog.menu(items, opts)` builds a selectable list leaf shaped
-- for a small, fixed set of choices: dim ` N. ` numbering, optional
-- description row per item (rendered dim under the label), digit-key
-- shortcuts (`1`..`9`), and a controller that talks in **1-based item
-- indices** so callers never have to compute a row stride.
--
-- Items may be strings (label-only) or `{ label, description?, key? }`
-- tables. If any item has a non-empty description the menu renders two
-- rows per item and cursor navigation steps by two so the cursor only
-- rests on label rows.
--
-- Shortcuts (`opts.shortcuts`, default `"submit"`):
--   * `"submit"` - pressing the item's digit moves the cursor to it AND
--                  fires the submit path (same as Enter on that row).
--   * `"select"` - digit moves the cursor; Enter still submits.
--   * `false`   - no digit handling; consumers can install their own.
--
-- Submit path: by default the menu resolves the active dialog with
-- `{ index = i, item = items[i] }`. Override via `opts.on_submit(ctx)`
-- - `ctx` exposes the standard dialog handles (`resolve`, `close`,
-- `win`, `panels`, `focused_leaf`) plus `ctx.index` (1-based) and
-- `ctx.item` for the selection - to map to a caller-specific payload
-- (e.g. confirm's `decisions[idx]`) or to defer (focus a sibling leaf
-- instead of resolving).
--
-- Returned `ctrl` exposes 1-based selection helpers:
--   ctrl:cursor()         -- currently selected index (1-based)
--   ctrl:cursor(i)        -- set selection (1-based; clamped)
--   ctrl:item()           -- currently selected item table
--   ctrl:items()          -- normalized item list
--   ctrl:size()           -- number of items
--   ctrl:submit()         -- trigger the submit path programmatically

local NS_MENU_NUM   = smelt.ns("smelt.dialog.menu.num")
local NS_MENU_LABEL = smelt.ns("smelt.dialog.menu.label")
local NS_MENU_DESC  = smelt.ns("smelt.dialog.menu.desc")

-- Render `items` into `buf`, applying dim numbering and the cursor-row
-- accent. `has_descriptions` toggles the two-row layout.
local function render_menu(buf, items, has_descriptions, numbered)
  local rendered = {}
  local meta = {}
  local desc_indent = "    "

  for i, it in ipairs(items) do
    local prefix = numbered and string.format(" %d. ", i) or " "
    local label_line = prefix .. (it.label or "")
    rendered[#rendered + 1] = label_line
    local label_row = #rendered

    local desc_row, desc_end
    if has_descriptions then
      local desc = it.description or ""
      local desc_line = desc_indent .. desc
      rendered[#rendered + 1] = desc_line
      desc_row = #rendered
      desc_end = #desc_line
    end

    meta[i] = {
      label_row   = label_row,
      label_start = #prefix,
      label_end   = #label_line,
      desc_row    = desc_row,
      desc_end    = desc_end,
    }
  end

  buf:lines(rendered):clear_ns(NS_MENU_NUM):clear_ns(NS_MENU_LABEL):clear_ns(NS_MENU_DESC)
  for _, m in ipairs(meta) do
    if numbered and m.label_start > 0 then
      buf:mark(NS_MENU_NUM, m.label_row, 0, { end_col = m.label_start, dim = true })
    end
    if m.label_end > m.label_start then
      buf:mark(NS_MENU_LABEL, m.label_row, m.label_start, {
        end_col       = m.label_end,
        hl_group      = "SmeltAccent",
        on_cursor_row = true,
      })
    end
    if m.desc_row and m.desc_end and m.desc_end > 0 then
      buf:mark(NS_MENU_DESC, m.desc_row, 0, { end_col = m.desc_end, dim = true })
    end
  end
end

-- Normalize `items` into `{ label, description?, key? }` tables.
local function normalize_items(items)
  local out = {}
  local has_descriptions = false
  for i, it in ipairs(items or {}) do
    local entry
    if type(it) == "string" then
      entry = { label = it }
    elseif type(it) == "table" then
      entry = {}
      for k, v in pairs(it) do entry[k] = v end
      entry.label = entry.label or ""
      if entry.description and entry.description ~= "" then
        has_descriptions = true
      end
    else
      entry = { label = tostring(it) }
    end
    out[i] = entry
  end
  return out, has_descriptions
end

--- Each item displayed in `smelt.dialog.menu`. Strings are also accepted
--- and lifted into this shape automatically.
---@class smelt.dialog.MenuItem
---@field label string Row text after the dim ` N. ` numbering.
---@field description? string Optional second row, rendered dim.
---@field key? string Optional chord that triggers this item (defaults to its 1-based index for items 1..9).

--- Options accepted by `smelt.dialog.menu`.
---@class smelt.dialog.MenuOpts
---@field selected? integer 1-based starting cursor (default 1).
---@field shortcuts? "submit"|"select"|false Digit-key behavior. Default `"submit"`.
---@field numbered? boolean Show the dim ` N. ` prefix (default true).
---@field on_submit? fun(ctx: any): any Override the submit path. `ctx` carries the dialog handles plus `ctx.index` (1-based) and `ctx.item`. Default resolves the active dialog with `{ index, item }`.

---@type fun(items: (string|smelt.dialog.MenuItem)[], opts: smelt.dialog.MenuOpts?): smelt.win.Win, table
function smelt.dialog.menu(items, opts)
  opts = opts or {}
  local normalized, has_descriptions = normalize_items(items)
  if #normalized == 0 then
    normalized = { { label = "" } }
  end
  local numbered  = opts.numbered ~= false
  local shortcuts = opts.shortcuts
  if shortcuts == nil then shortcuts = "submit" end

  local row_stride = has_descriptions and 2 or 1
  local item_count = #normalized
  local max_row    = (item_count - 1) * row_stride

  local selected = tonumber(opts.selected or 1) or 1
  if selected < 1 then selected = 1 end
  if selected > item_count then selected = item_count end

  local buf = smelt.buf.new()
  render_menu(buf, normalized, has_descriptions, numbered)

  local leaf = smelt.win.new(buf, {
    region         = REGION,
    surface       = "list",
    pad_left       = GUTTER,
    pad_right      = GUTTER,
    scrollbar      = false,
    kind           = "list",
    initial_cursor = (selected - 1) * row_stride,
  })

  local function row_of(i) return (i - 1) * row_stride end
  local function index_of_row(r) return math.floor(r / row_stride) + 1 end

  local ctrl = {}
  function ctrl:cursor(i)
    if i == nil then
      return index_of_row(leaf:cursor() or 0)
    end
    if i < 1 then i = 1 end
    if i > item_count then i = item_count end
    leaf:cursor(row_of(i))
    return self
  end
  function ctrl:item() return normalized[self:cursor()] end
  function ctrl:items() return normalized end
  function ctrl:size() return item_count end

  local function default_on_submit(ctx)
    if ctx.resolve then
      ctx.resolve({ index = ctx.index, item = ctx.item })
    end
  end

  -- Builds the submit ctx by layering `index`/`item` over the active
  -- dialog's ctx so handlers get one consistent shape (matches
  -- `dialog.open`'s `on_submit(ctx)` argument).
  local function submit_at(i)
    if i < 1 or i > item_count then return end
    leaf:cursor(row_of(i))
    local dlg = smelt.dialog.current() or {}
    local ctx = {
      win          = dlg.win,
      panels       = dlg.panels,
      focused_leaf = dlg.focused_leaf,
      resolve      = dlg.resolve,
      close        = dlg.close,
      index        = i,
      item         = normalized[i],
    }
    local handler = opts.on_submit or default_on_submit
    local ok, err = pcall(handler, ctx)
    if not ok then report_callback_error("menu submit", err) end
  end

  function ctrl:submit() submit_at(self:cursor()) end

  -- Multi-row stride: step the cursor by `row_stride` so it never lands
  -- on a description row. Single-row menus keep the built-in list
  -- navigation untouched.
  if row_stride > 1 then
    local function step(units)
      return function()
        local cur = leaf:cursor() or 0
        local target = cur + units * row_stride
        if target < 0 then target = 0 end
        if target > max_row then target = max_row end
        leaf:cursor(target)
      end
    end
    leaf:key("up",   step(-1))
    leaf:key("down", step(1))
    leaf:key("k",    step(-1))
    leaf:key("j",    step(1))
    leaf:key("c-k",  step(-1))
    leaf:key("c-j",  step(1))
    leaf:key("c-p",  step(-1))
    leaf:key("c-n",  step(1))
    leaf:key("pgup", step(-10))
    leaf:key("pgdn", step(10))
    leaf:key("c-u",  step(-5))
    leaf:key("c-d",  step(5))
  end

  -- Enter funnels through the same submit path as digit shortcuts.
  leaf:key("enter", function() submit_at(ctrl:cursor()) end)

  -- Digit shortcuts. `key = "X"` on an item overrides its digit binding;
  -- without an override items 1..9 use their 1-based index. Bindings live
  -- on the leaf (tier 1a) so typing digits into a sibling input leaf
  -- still inserts the character.
  if shortcuts then
    local function chord_for(i, item)
      if item.key and item.key ~= "" then return item.key end
      if i <= 9 then return tostring(i) end
      return nil
    end
    for i, item in ipairs(normalized) do
      local chord = chord_for(i, item)
      if chord then
        if shortcuts == "submit" then
          leaf:key(chord, function() submit_at(i) end)
        else
          leaf:key(chord, function() leaf:cursor(row_of(i)) end)
        end
      end
    end
  end

  return leaf, ctrl
end

-- Wrap an existing `buf` as a selectable list leaf. Use when the buffer
-- contents need to be mutated live (vs. the snapshot supplied to
-- `smelt.dialog.menu`). `opts.surface` defaults to `"list"`; `opts.selected`
-- (0-based) sets the initial cursor row.
---@type fun(buf: smelt.buf.Buf, opts: table?): smelt.win.Win
function smelt.dialog.list(buf, opts)
  opts = opts or {}
  local surface = opts.surface or "list"
  local leaf = smelt.win.new(buf, {
    region = REGION, surface = surface,
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

-- Render `text` as a non-focusable markdown leaf. Convenience wrapper
-- around `smelt.dialog.content` for static narrative panels (notes,
-- summaries, intros). Returns `(leaf, buf)`.
---@type fun(text: string): smelt.win.Win, smelt.buf.Buf
function smelt.dialog.markdown(text)
  local buf = smelt.buf.new({ mode = "markdown" })
  buf:source(text or "")
  local leaf = smelt.win.new(buf, {
    region = REGION, surface = "selectable_text",
    pad_left = GUTTER, pad_right = GUTTER,
  })
  return leaf, buf
end

-- General-purpose body leaf. Pass `opts.buf` to wrap an existing buffer
-- or `opts.text` to spin up a fresh read-only one. `opts.readonly` can
-- force the backing buffer's readonly flag when a caller supplies `opts.buf`.
-- `opts.interactive` enables focus + vim keymaps (when the user has vim mode
-- on); `opts.wrap` mirrors `smelt.win.new`. Returns `(leaf, buf)`.
---@type fun(opts: table?): smelt.win.Win, smelt.buf.Buf
function smelt.dialog.content(opts)
  opts = opts or {}
  local buf = opts.buf
  if not buf then
    buf = smelt.buf.new({ readonly = true })
    if opts.text and opts.text ~= "" then
      buf:lines(split_lines(opts.text))
    end
  end
  if opts.readonly ~= nil then
    buf:readonly(opts.readonly and true or false)
  end
  local surface = opts.surface
  if surface == nil then surface = opts.interactive and "readonly_text" or "selectable_text" end
  -- `wrap` defaults to true (matches `smelt.win.new`); pass `wrap = false` to
  -- show pre-styled content (e.g. via `buf:styled(...)`) at its
  -- intrinsic width without soft-wrapping the row.
  local leaf = smelt.win.new(buf, {
    region      = REGION,
    surface     = surface,
    vim_enabled = (opts.interactive and smelt.settings.vim) and true or false,
    pad_left    = GUTTER,
    pad_right   = GUTTER,
    wrap        = opts.wrap,
  })
  return leaf, buf
end

local function viewer_keymaps(extra)
  local keymaps = {
    { key = "q", on_press = function(ctx) ctx.close() end },
    { key = "?", on_press = function(ctx) ctx.close() end },
  }
  for _, km in ipairs(extra or {}) do keymaps[#keymaps + 1] = km end
  return keymaps
end

--- Open a read-only content dialog. Pass `text` for plain text, `lines` for
--- plain line tables, `styled` for styled lines, or `buf` for a live buffer the
--- caller will update after opening. Returns `(handle, buf, leaf)`.
---@type fun(opts: table): table, smelt.buf.Buf, smelt.win.Win
function smelt.dialog.viewer(opts)
  opts = opts or {}
  local buf = opts.buf or smelt.buf.new({ readonly = true })
  if opts.styled then
    buf:styled(opts.styled)
  elseif opts.lines then
    buf:lines(opts.lines)
  elseif opts.text ~= nil then
    buf:lines(split_lines(opts.text or ""))
  end

  local leaf = smelt.dialog.content({
    buf         = buf,
    interactive = opts.interactive ~= false,
    wrap        = opts.wrap,
    surface     = opts.surface,
  })

  local panel = { leaf = leaf, height = opts.panel_height }
  local max_height = opts.max_height
  if opts.height == nil and max_height == nil then max_height = "50%" end
  local handle = smelt.dialog.open_handle({
    title      = opts.title,
    height     = opts.height,
    max_height = max_height,
    min_height = opts.min_height,
    panels     = { panel },
    keymaps    = viewer_keymaps(opts.keymaps),
  })
  return handle, buf, leaf
end

-- ── Dialog overlay wrapper ────────────────────────────────────────────

-- Build the overlay items table from panels and open the overlay. Returns
-- the root leaf and the array of leaves.
--
-- Dialog height (pick one; setting both is an error):
--   * `opts.height`     - fixed size: integer cells, `"N%"`, `"fill"`. Default `"60%"`.
--   * `opts.max_height` - shrink to content, capped at this size.
--   * `opts.min_height` - floor that pairs with either mode. Fit-mode dialogs
--                         default to `min_height = "30%"` so a placeholder
--                         body stays visible when content collapses; pass
--                         `min_height = 0` to opt out.
--
-- All three knobs are **body-relative** when given as integer cells: the
-- wrapper adds the dialog's chrome (top border + title row, 1 cell) before
-- forwarding to the overlay (which uses total-rect semantics). `"N%"` /
-- `"fill"` / `"fit"` are forwarded verbatim - percentages of the terminal
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

local function copy_title_span(span)
  if type(span) == "string" then
    return { text = span, dim = true }
  end
  if type(span) ~= "table" then return span end
  local out = {}
  for k, v in pairs(span) do out[k] = v end
  out.dim = true
  return out
end

--- Return a title spec styled for dialog/window chrome. Use this for raw
--- overlays or layout leaves that should match dialog title treatment;
--- `smelt.dialog.open` applies it automatically to its own title and panels.
---@type fun(title: string|table, opts?: { pad?: boolean }): table|string|nil
function smelt.dialog.title(title, opts)
  opts = opts or {}
  if title == nil or title == "" then return title end

  local spans
  if type(title) == "string" then
    spans = { { text = title, dim = true } }
  elseif type(title) == "table" then
    if title.text ~= nil then
      spans = { copy_title_span(title) }
    elseif #title > 0 then
      spans = {}
      for _, span in ipairs(title) do spans[#spans + 1] = copy_title_span(span) end
    else
      return title
    end
  else
    return title
  end

  if opts.pad then
    table.insert(spans, 1, { text = " " })
    spans[#spans + 1] = { text = " " }
  end
  return spans
end

local function open_overlay(opts)
  if opts.height ~= nil and opts.max_height ~= nil then
    error("smelt.dialog: use `height` (fixed) or `max_height` (fit to content), not both", 3)
  end
  local fit_mode = opts.max_height ~= nil
  local default_panel_height = fit_mode and "fit" or nil
  -- Fit-mode dialogs read their natural size - for trivial content (one
  -- placeholder line) that collapses to just the chrome row. Default to a
  -- 30% terminal-height floor so the placeholder + a comfortable margin stay
  -- visible. Callers can override via `opts.min_height` (including `0` to opt
  -- out entirely).
  local default_min_height = fit_mode and "30%" or nil

  local top_panels = opts.panels or {}
  local bottom_panels = opts.bottom_panels or {}
  if #top_panels == 0 and #bottom_panels == 0 then
    error("smelt.dialog: panels or bottom_panels must be non-empty", 3)
  end

  local leaves = {}
  local function build_layout_items(panels, start_index)
    local layout_items = {}
    for i, p in ipairs(panels) do
      if type(p) ~= "table" or p.leaf == nil then
        error("smelt.dialog: panel " .. (start_index + i - 1) .. " requires a `leaf`", 3)
      end
      leaves[#leaves + 1] = p.leaf
      local leaf_node = smelt.ui.layout.leaf(p.leaf, {
        border              = p.border,
        title               = smelt.dialog.title(p.title),
        collapse_when_empty = p.collapse_when_empty or false,
      })
      layout_items[i] = { leaf_node, height = p.height or default_panel_height }
    end
    return layout_items
  end

  local top_items = build_layout_items(top_panels, 1)
  local bottom_items = build_layout_items(bottom_panels, #top_panels + 1)

  -- The wrapper is responsible for the single-cell gutter on each side of the
  -- title content. Callers MUST NOT pad - pass `"messages"` not `" messages "`,
  -- and for multi-span titles drop the leading space on the first span and the
  -- trailing space on the last span.
  local title = smelt.dialog.title(opts.title, { pad = true })

  local panel_vbox
  if #top_items > 0 and #bottom_items > 0 then
    panel_vbox = smelt.ui.layout.vbox({
      { smelt.ui.layout.vbox(top_items),    height = "fit" },
      { smelt.ui.layout.vbox(bottom_items), height = "fit" },
    }, { gap = opts.bottom_gap or 0, justify = "space-between" })
  else
    panel_vbox = smelt.ui.layout.vbox(#top_items > 0 and top_items or bottom_items)
  end

  -- fixed: width = "100%", height = opts.height (or "60%" default)
  -- fit:   width = "100%", height = "fit", max_height = opts.max_height
  local height_spec, max_height_spec
  if opts.max_height ~= nil then
    height_spec, max_height_spec = "fit", opts.max_height
  else
    height_spec, max_height_spec = (opts.height or "60%"), nil
  end

  -- Reserve rows for the Lua-allocated statusline so the dialog docks
  -- above it instead of overlapping. The host has no statusline concept
  -- of its own; `statusline.rows` is the composer's self-reported row
  -- count (the window's `:rect()` isn't usable on cold start because
  -- the layout hasn't placed it yet).
  local statusline = require("smelt.statusline")
  local overlay = smelt.overlay.new({
    title        = title,
    anchor       = "dock_bottom",
    above_rows   = statusline.rows or 0,
    border       = opts.border or { top = "SmeltAccent" },
    modal        = true,
    blocks_agent = opts.blocks_agent or false,
    draggable    = false,
    resizable    = opts.resizable == false and false or { top = true },
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

  -- Shared dialog ctx. `focused_leaf` is mutated live by the focus event
  -- handlers below so callbacks always read the current value. Exposed via
  -- `smelt.dialog.current()` and as the `ctx` arg to `opts.keymaps` /
  -- `on_submit` handlers.
  local ctx = {
    win          = root,
    panels       = leaves,
    focused_leaf = opts.focus,
  }

  local make_ctx
  local resolved = false
  local function resolve(value)
    if resolved then return end
    resolved = true
    if type(opts.on_close) == "function" then
      local ok, err = pcall(opts.on_close, make_ctx())
      if not ok then report_callback_error("on_close", err) end
    end
    -- Pop our entry off the dialog stack. We scan the stack from the top
    -- in case nested dialogs resolve out of order (a child dialog closes
    -- last) - only remove our own entry.
    for i = #dialog_stack, 1, -1 do
      if dialog_stack[i] == ctx then
        table.remove(dialog_stack, i)
        break
      end
    end
    root:close()
    resolve_fn(value)
  end
  ctx.resolve = resolve
  ctx.close   = function() resolve(nil) end

  table.insert(dialog_stack, ctx)

  for _, leaf in ipairs(leaves) do
    leaf:on("focus", function()
      ctx.focused_leaf = leaf
    end)
  end

  -- Build a ctx for user callbacks. Raw event fields (`text`, `index`, `code`,
  -- `mods`, `leaf`) flow through unchanged; the shared dialog ctx is layered
  -- underneath so callbacks see `resolve`, `close`, `panels`, `focused_leaf`.
  make_ctx = function(raw_ctx)
    local out = {
      win          = ctx.win,
      panels       = ctx.panels,
      focused_leaf = ctx.focused_leaf,
      resolve      = ctx.resolve,
      close        = ctx.close,
    }
    if type(raw_ctx) == "table" then
      for k, v in pairs(raw_ctx) do out[k] = v end
    end
    return out
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
          if not ok then report_callback_error("keymap", err) end
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
  -- open - Enter doing nothing is easier to diagnose than Enter mysteriously
  -- closing. Esc/Ctrl-C still dismisses via the Dismiss event below.
  if type(opts.on_submit) == "function" then
    register_on_all("submit", function(raw_ctx)
      local ok, err = pcall(opts.on_submit, make_ctx(raw_ctx))
      if not ok then report_callback_error("on_submit", err) end
    end)
  end

  -- Dismiss: Esc / Ctrl-C / outside-modal click. Defaults to resolve(nil).
  register_on_all("dismiss", function(raw_ctx)
    if type(opts.on_dismiss) == "function" then
      local ok, err = pcall(opts.on_dismiss, make_ctx(raw_ctx))
      if not ok then report_callback_error("on_dismiss", err) end
    else
      resolve(nil)
    end
  end)

  if type(opts.on_tick) == "function" then
    register_on_all("tick", function(raw_ctx)
      local ok, err = pcall(opts.on_tick, make_ctx(raw_ctx))
      if not ok then report_callback_error("on_tick", err) end
    end)
  end

  if type(opts.on_event) == "table" then
    for event_name, fn in pairs(opts.on_event) do
      if type(fn) == "function" then
        register_on_all(event_name, function(raw_ctx)
          local ok, err = pcall(fn, make_ctx(raw_ctx))
          if not ok then report_callback_error("on_event[" .. event_name .. "]", err) end
        end)
      end
    end
  end

  return resolve, root
end

-- Coroutine-blocking dialog opener. Builds the overlay from `opts.panels`
-- (each `{ leaf, height }`), wires `opts.keymaps`, then yields the
-- caller until a handler calls `ctx.resolve(value)`. Must run inside a
-- `smelt.spawn` (or tool execute) frame; returns the resolved value or
-- `nil` on dismiss.
---@type fun(opts: smelt.dialog.Opts): any
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
  return smelt.task.wait(task_id, { interactive = true })
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
--   * `items`       - array of arbitrary item tables (passed to `render`).
--   * `render`      - `function(item) -> { text = ..., marks = ... }` (see
--                     `smelt.list`).
--   * `filter`      - optional predicate `function(item) -> bool` applied
--                     to every refilter; the picker re-runs it whenever
--                     the query changes (so it can close over the live
--                     query state).
--   * `placeholder` - input prompt text. Defaults to `""`.
--   * `empty_text`  - shown in the list when nothing matches.
--   * `on_open`     - `function(ctx)` fires once before the dialog blocks,
--                     after the input/list have been built. Use it to seed
--                     marks on the input buffer or to set an initial cursor
--                     row on the list.
--   * `on_query`    - `function(query, ctx)` fires on every keystroke.
--                     The default is `list:set_filter(opts.filter)`. Pass
--                     this when you want to swap the filter (e.g. rebuild
--                     it from a fresh query).
--   * `on_submit`   - `function(ctx)` fires on Enter. `ctx.item` is the
--                     highlighted row (nil when the list is empty);
--                     `ctx.list`/`ctx.input`/`ctx.input_buf` are added
--                     by the picker. Default resolves with `ctx.item`
--                     when non-nil.
--   * `keymaps`     - extra dialog-level keymaps merged on top of the
--                     built-in navigation bindings. Each entry's
--                     `on_press(ctx)` receives the picker ctx with
--                     `ctx.list`, `ctx.input`, `ctx.input_buf` added.
--   * `title`, `height`, `max_height`, `min_height`, `blocks_agent` - forwarded to
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

-- Coroutine-blocking Telescope-style picker. Stacks a single-line input
-- on top of a list driven by `smelt.list.new`; navigation forwards from
-- input to list, Enter resolves with the selected item. See the doc
-- block above `NAV_KEYS` for every accepted `opts` field. Returns the
-- value resolved from `on_submit` (defaults to the highlighted item) or
-- `nil` on dismiss.
---@type fun(opts: smelt.dialog.PickerOpts): any
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
  local list_leaf = smelt.dialog.list(list_buf, { surface = "list_inert" })

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
    on_submit = function(ctx)
      ctx.item = list:selected()
      return opts.on_submit(augment(ctx))
    end
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

-- Non-coroutine open. Returns `{ win, panels, close() }` synchronously.
-- The consumer drives the lifecycle via `on_submit` / `on_dismiss`
-- callbacks and tears down with `handle:close()`. No value flows back
-- - use `smelt.dialog.open` when you need to read the result.
---@type fun(opts: smelt.dialog.Opts): table
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
