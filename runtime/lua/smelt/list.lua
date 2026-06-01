-- `smelt.list`: structured-list helper over a `smelt.dialog.list` leaf.
--
-- The dialog list primitive is a generic line buffer with a cursor. This
-- helper layers over it the bookkeeping a typical picker needs: a list of
-- arbitrary item tables, a per-item render function, an optional filter,
-- and a single mapping between the cursor row and the original item. Drop
-- to raw `dialog.list` whenever you need finer control.
--
-- Usage:
--   local leaf = smelt.dialog.list(buf, { focusable = false })
--   local list = smelt.list.new({
--     leaf = leaf, buf = buf, items = entries,
--     render = function(it)
--       return {
--         text  = string.format("%s%s", string.rep("  ", it.depth or 0), it.label),
--         marks = { { col = 0, end_col = 8, opts = { dim = true } } },
--       }
--     end,
--     filter = function(it) return it.cwd == current_cwd end,
--     empty_text = "  (no matches)",
--   })
--
--   list:set_filter(function(it) ... end)   -- swaps the predicate + refresh
--   list:set_items_preserve(items, function(it) return it.id end)
--   list:refresh()                          -- re-derive visible + re-render
--   local item = list:selected()            -- or nil
--   list:move_cursor(1)
--
-- Text rendered by `render(item).text` is truncated with `smelt.text.fit`
-- to the leaf's `content_width()` so long items don't trigger horizontal
-- panning. Return `spans = { { text, style?, syntax? }, ... }` to render the
-- fitted row through `buf:styled`, including inline syntax highlighting.
-- The list re-renders automatically when the leaf is resized.

smelt.list = smelt.list or {}

local NS = smelt.ns("smelt.list.marks")

local List = {}
List.__index = List

local function clone_marks(marks)
  if not marks then return nil end
  local out = {}
  for i, m in ipairs(marks) do out[i] = m end
  return out
end

local function span_text(spans)
  local out = {}
  for i, span in ipairs(spans or {}) do out[i] = span.text or "" end
  return table.concat(out)
end

local function fit_spans(spans, width)
  if not spans then return nil end
  if not width or width <= 0 then return spans end

  local fitted = {}
  local remaining = width
  for _, span in ipairs(spans) do
    if remaining <= 0 then break end
    local text = span.text or ""
    local span_width = smelt.text.width(text)
    local part = text
    if span_width > remaining then
      part = smelt.text.fit(text, remaining)
    end
    if part ~= "" then
      local out = { text = part }
      if span.style then out.style = span.style end
      if span.syntax then out.syntax = span.syntax end
      fitted[#fitted + 1] = out
    end
    remaining = remaining - smelt.text.width(part)
  end
  if remaining > 0 then
    fitted[#fitted + 1] = { text = string.rep(" ", remaining) }
  end
  return fitted
end

local function plain_spans(text)
  return { { text = text } }
end

-- Read the leaf's inner-content width in cells (gutter and pad already
-- subtracted). Returns nil before the first paint — the leaf has no
-- viewport until then.
local function content_width(self)
  return self.leaf:content_width()
end

local function render_visible(self)
  self.buf:clear_ns(NS)
  local visible = self.visible_items
  if #visible == 0 then
    self.buf:lines({ self.empty_text })
    self.buf:mark(NS, 1, 0, { end_col = #self.empty_text, dim = true })
    self.last_rendered_width = content_width(self)
    return
  end
  local width = content_width(self)
  local lines = {}
  local styled_lines = {}
  local row_marks = {}
  local use_styled = false
  for i, item in ipairs(visible) do
    local rendered = self.render(item) or {}
    local text = rendered.text or span_text(rendered.spans)
    -- Truncate to the leaf's content width so long items don't trigger
    -- horizontal panning. `fit` pads to exact width — trailing whitespace
    -- is invisible and keeps the selection-highlight row uniform.
    if width and width > 0 then
      text = smelt.text.fit(text, width)
    end
    lines[i] = text
    if rendered.spans then
      use_styled = true
      styled_lines[i] = fit_spans(rendered.spans, width)
    else
      styled_lines[i] = plain_spans(text)
    end
    row_marks[i] = clone_marks(rendered.marks)
  end
  if use_styled then
    self.buf:styled(styled_lines)
  else
    self.buf:lines(lines)
  end
  for i, marks in ipairs(row_marks) do
    if marks then
      for _, m in ipairs(marks) do
        self.buf:mark(NS, i, m.col or 0, m.opts or {})
      end
    end
  end
  self.last_rendered_width = width
end

local function rederive_visible(self)
  if not self.filter then
    self.visible_items = self.items
    return
  end
  local out = {}
  for _, it in ipairs(self.items) do
    if self.filter(it) then table.insert(out, it) end
  end
  self.visible_items = out
end

function List:refresh()
  rederive_visible(self)
  render_visible(self)
  self.leaf:cursor(0)
end

function List:set_items(items)
  self.items = items or {}
  self:refresh()
end

function List:set_items_preserve(items, key_fn)
  if type(key_fn) ~= "function" then
    error("smelt.list: key function is required", 2)
  end
  local selected = self:selected()
  local selected_key = selected and key_fn(selected)
  self.items = items or {}
  self:refresh()
  if selected_key == nil then return end
  for i, item in ipairs(self.visible_items) do
    if key_fn(item) == selected_key then
      self.leaf:cursor(i - 1)
      return
    end
  end
end

function List:set_filter(fn)
  self.filter = fn
  self:refresh()
end

function List:set_render(fn)
  if type(fn) ~= "function" then
    error("smelt.list: render must be a function", 2)
  end
  self.render = fn
  render_visible(self)
end

function List:visible()
  return self.visible_items
end

function List:size()
  return #self.visible_items
end

function List:selected_index()
  if #self.visible_items == 0 then return nil end
  return self.leaf:cursor() or 0
end

function List:selected()
  local idx = self:selected_index()
  if not idx then return nil end
  return self.visible_items[idx + 1]
end

function List:set_cursor(i)
  self.leaf:cursor(i)
end

function List:move_cursor(delta)
  self.leaf:move_cursor(delta)
end

--- Options accepted by `smelt.list.new`. `leaf` and `buf` are mandatory —
--- they own the rendered selection cursor and the backing line buffer;
--- the rest configure how data is sourced, filtered, and rendered.
---@class smelt.list.Opts
---@field leaf smelt.win.Win Selectable list leaf (typically from `smelt.dialog.list`).
---@field buf smelt.buf.Buf Backing buffer that mirrors the rendered rows.
---@field items? any[] Initial item set. Mutate via `:set_items(...)` later if needed.
---@field render fun(item: any): table Returns `{ text, spans?, marks? }` per visible row.
---@field filter? fun(item: any): boolean Predicate re-run on `:set_filter` / `:refresh`.
---@field empty_text? string Placeholder line shown when no row passes the filter.

-- Build a structured list bound to the dialog-list `opts.leaf` and its
-- backing `opts.buf`. `opts.items` is the data source; `opts.render(item)`
-- returns `{ text, spans?, marks }`; `opts.filter(item)` is optional and re-runs
-- whenever `:set_filter` / `:refresh` fires. `opts.empty_text` shows
-- when no row passes the filter. Returns a handle with `:selected`,
-- `:set_items`, `:set_items_preserve`, `:set_filter`, `:refresh`,
-- `:set_cursor`, `:move_cursor`. See the header docstring for the full
-- usage shape.
---@type fun(opts: smelt.list.Opts): table
function smelt.list.new(opts)
  if type(opts) ~= "table" then
    error("smelt.list.new: expected options table", 2)
  end
  if opts.leaf == nil then error("smelt.list.new: opts.leaf is required", 2) end
  if opts.buf == nil then error("smelt.list.new: opts.buf is required", 2) end
  if type(opts.render) ~= "function" then
    error("smelt.list.new: opts.render must be a function", 2)
  end

  local self = setmetatable({
    leaf          = opts.leaf,
    buf           = opts.buf,
    render        = opts.render,
    filter        = opts.filter,
    items         = opts.items or {},
    visible_items = {},
    empty_text    = opts.empty_text or "  (no items)",
    last_rendered_width = nil,
  }, List)
  self:refresh()
  -- Re-render when the leaf's content width changes. `content_width()` is
  -- nil at construction (no viewport until first paint), so the first
  -- `resized` event fires the real-width render and any later terminal
  -- resize re-triggers it.
  self.resize_reg = self.leaf:on("resized", function()
    render_visible(self)
  end)
  return self
end
