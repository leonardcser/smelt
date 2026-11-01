-- Bars: rows of items gathered from named sources.
--
-- A bar is a registry of named sources. A source is a function returning
-- items; a repaint calls every source in order, then lays the items out for
-- the bar's width. Two layouts share the item shape, the priority-drop fitter
-- and the spacing rules:
--
--   * "strip": filled rows with the left items and the right items packed
--     against opposite edges. Items pick their row with `row`, and the bar
--     takes one row per distinct `row` value that has a visible item.
--   * "rule":  a horizontal rule with the left items and the right items set
--     into it. One row.
--
-- `smelt.bar.new(opts)` returns a `smelt.bar.Bar` that owns its window and
-- renderer and places into any layout with `bar:node()`. The statusline, the
-- headerline and the bars around the prompt are such bars.
--
-- `style` is a subset of `smelt.buf.MarkOpts` (`fg`, `bg`, `hl_group`, `bold`,
-- `dim`, `italic`). `fg` / `bg` take any color form, including a theme group
-- name. The same table is forwarded verbatim to `buf:mark`.
--
-- A laid-out row is `{ text, highlights }` where each highlight carries
-- `bytes_start` / `bytes_end` (byte offsets, the unit `buf:mark` expects), the
-- forwarded `style`, and optional `selectable` metadata.

local M = {}

local DEFAULT_RULE = "\u{2500}"
local DEFAULT_SEPARATOR = " \u{00b7} "
local DEFAULT_RULE_STYLE = { fg = "SmeltSeparator" }
local RULE_MIN_CELLS = 4

--- One piece of bar content. Items are laid out left to right, with
--- `align_right` items packed against the right edge. Give an item either
--- `text`, or `runs` to color its characters individually. Neighbouring items
--- get one space between them, and none between two items that both carry a
--- background. When the bar is too narrow, whole items drop from the highest
--- `priority` down, and truncatable items of the highest remaining priority
--- shrink first. An item whose text is empty is skipped.
---@class smelt.bar.Item
---@field text? string Text to show. Ignored when `runs` is set.
---@field runs? smelt.bar.Run[] Consecutive styled pieces of one item; they drop together and are never truncated.
---@field style? smelt.buf.MarkOpts Highlight applied to the text (`fg`, `bg`, `hl_group`, `bold`, `dim`, `italic`).
---@field priority? integer Drop order when space runs out; higher values drop first, and 0 is never dropped. Defaults to 0.
---@field separated? boolean Draw a `·` between this item and the previous separated item on the same side.
---@field truncatable? boolean Allow shrinking this item before dropping it.
---@field truncate? "end"|"middle" Where to cut a truncated item. Defaults to `"end"`.
---@field align_right? boolean Pack against the right edge instead of the left.
---@field selectable? boolean Include the text in drag-copy. Defaults to true.
---@field row? integer Orders the rows of a strip: rows with lower numbers sit higher, negative rows count from the bottom (`-1` is the last row), and a row with no items takes no space. Defaults to 1. A rule ignores it.
---@field fill? string Theme group that styles the empty space of this item's row instead of the bar's background.

--- What a source gets on every repaint.
---@class smelt.bar.Context
---@field width integer The bar's width in cells.
---@field rows? integer Rows left for this source while the layout gives the bar fewer rows than it wants: the bar's rows minus the rows earlier sources took. Nil when every row fits.

--- A styled piece of a `runs` item.
---@class smelt.bar.Run
---@field text string Text to show.
---@field style? smelt.buf.MarkOpts Highlight applied to the text.
---@field selectable? boolean Include the text in drag-copy. Defaults to the item's setting.

-- ── rows ────────────────────────────────────────────────────────────

local function segments_to_line(segs)
  local parts, highlights = {}, {}
  local byte_pos = 0
  for _, seg in ipairs(segs) do
    local len = #seg.text
    if len > 0 then
      parts[#parts + 1] = seg.text
      highlights[#highlights + 1] = {
        bytes_start = byte_pos,
        bytes_end = byte_pos + len,
        style = seg.style or {},
        selectable = seg.selectable,
      }
      byte_pos = byte_pos + len
    end
  end
  return { text = table.concat(parts), highlights = highlights }
end

local function write_rows(buf, rows, ns)
  local lines = {}
  for i, row in ipairs(rows) do
    lines[i] = row.text
  end
  buf:lines(lines)
  buf:clear_ns(ns)
  for i, row in ipairs(rows) do
    for _, hl in ipairs(row.highlights or {}) do
      if hl.bytes_end > hl.bytes_start then
        local opts = {}
        for k, v in pairs(hl.style) do opts[k] = v end
        opts.end_col = hl.bytes_end
        if hl.selectable ~= nil then opts.selectable = hl.selectable end
        buf:mark(ns, i, hl.bytes_start, opts)
      end
    end
  end
end

-- ── fitting ─────────────────────────────────────────────────────────

-- Entries wrap items with their measured width. Truncation rewrites the entry,
-- never the item, so a source can return the same tables on every repaint.
local function entries_of(items)
  local entries = {}
  for _, item in ipairs(items) do
    local w = 0
    if item.runs then
      for _, run in ipairs(item.runs) do w = w + smelt.text.width(run.text) end
    elseif item.text then
      w = smelt.text.width(item.text)
    end
    if w > 0 then
      entries[#entries + 1] = {
        item = item,
        text = item.text,
        w = w,
        priority = item.priority or 0,
        right = item.align_right or false,
      }
    end
  end
  return entries
end

local function has_block_bg(entry)
  local style = entry.item.style
  return style ~= nil and (style.bg ~= nil or style.hl_group ~= nil)
end

local function gap_between(prev, entry)
  if has_block_bg(prev) and has_block_bg(entry) then return 0 end
  return 1
end

-- Cells and item count of one side, including the gaps and separators the
-- join below will insert.
local function side_size(entries, right, sep_w)
  local w, count, sep_seen, prev = 0, 0, false, nil
  for _, e in ipairs(entries) do
    if e.right == right then
      if prev then
        if e.item.separated and sep_seen then
          w = w + sep_w
        else
          w = w + gap_between(prev, e)
        end
      end
      if e.item.separated then sep_seen = true end
      w = w + e.w
      count = count + 1
      prev = e
    end
  end
  return w, count
end

local function fit_middle(text, cells)
  if smelt.text.width(text) <= cells then return text end
  if cells == 0 then return "" end
  if cells == 1 then return "…" end

  local first, last = text:match("^([^/]+)/.+/([^/]+)$")
  if first and last then
    local path_mid = first .. "/…/" .. last
    if smelt.text.width(path_mid) <= cells then return path_mid end
  end

  local chars = {}
  for _, code in utf8.codes(text) do
    chars[#chars + 1] = utf8.char(code)
  end

  local left_w = math.floor((cells - 1) / 2)
  local right_w = cells - 1 - left_w
  local left, used = {}, 0
  for _, ch in ipairs(chars) do
    local w = smelt.text.width(ch)
    if used + w > left_w then break end
    left[#left + 1] = ch
    used = used + w
  end

  local right_rev = {}
  used = 0
  for i = #chars, 1, -1 do
    local ch = chars[i]
    local w = smelt.text.width(ch)
    if used + w > right_w then break end
    right_rev[#right_rev + 1] = ch
    used = used + w
  end
  local right = {}
  for i = #right_rev, 1, -1 do right[#right + 1] = right_rev[i] end

  return table.concat(left) .. "…" .. table.concat(right)
end

local function fit_text(entry, cells)
  if entry.item.truncate == "middle" then return fit_middle(entry.text, cells) end
  return smelt.text.fit(entry.text, cells, { suffix = "…" })
end

-- Shrink `entries` until `measure(entries)` fits `width`. The highest priority
-- goes first: a truncatable text item at that priority shrinks to the room
-- left, otherwise the whole priority level drops. Priority 0 never drops.
local function fit(entries, width, measure)
  while #entries > 0 and measure(entries) > width do
    local top = 0
    for _, e in ipairs(entries) do
      if e.priority > top then top = e.priority end
    end
    if top == 0 then break end

    local shrunk = false
    for i = #entries, 1, -1 do
      local e = entries[i]
      if e.priority == top and e.item.truncatable and not e.item.runs then
        local avail = width - (measure(entries) - e.w)
        if avail >= 2 then
          local text = fit_text(e, avail)
          local w = smelt.text.width(text)
          if w < e.w then
            e.text, e.w = text, w
            shrunk = true
          end
        end
        break
      end
    end

    if not shrunk then
      local kept = {}
      for _, e in ipairs(entries) do
        if e.priority ~= top then kept[#kept + 1] = e end
      end
      entries = kept
    end
  end
  return entries
end

-- ── joining ─────────────────────────────────────────────────────────

-- Append the runs of one side: items in order, one space between neighbours
-- and `sep` between consecutive separated items.
local function join_side(entries, right, out, style_of, gap_style, sep, sep_style, filler_selectable)
  filler_selectable = filler_selectable == true
  local sep_seen, prev = false, nil
  for _, e in ipairs(entries) do
    if e.right == right then
      if prev then
        if e.item.separated and sep_seen then
          -- Between two block items the dot sits on their background.
          local style = sep_style
          if has_block_bg(prev) and has_block_bg(e) then style = style_of(e.item.style) end
          out[#out + 1] = { text = sep, style = style, selectable = filler_selectable }
        elseif gap_between(prev, e) > 0 then
          out[#out + 1] = { text = " ", style = gap_style, selectable = filler_selectable }
        end
      end
      if e.item.separated then sep_seen = true end
      if e.item.runs then
        for _, run in ipairs(e.item.runs) do
          local selectable = run.selectable
          if selectable == nil then selectable = e.item.selectable end
          out[#out + 1] = {
            text = run.text,
            style = style_of(run.style or e.item.style),
            selectable = selectable,
          }
        end
      else
        out[#out + 1] = {
          text = e.text,
          style = style_of(e.item.style),
          selectable = e.item.selectable,
        }
      end
      prev = e
    end
  end
end

-- Cells kept between the two sides of a strip: none when the facing items
-- both carry a background, like neighbours on one side.
local function side_gap(entries)
  local last_left, first_right
  for _, e in ipairs(entries) do
    if e.right then
      first_right = first_right or e
    else
      last_left = e
    end
  end
  if last_left and first_right then return gap_between(last_left, first_right) end
  return 1
end

-- A copy of `style` backed by `bg`. `hl_group` carries its own bg, so an item
-- that picked one (a mode pill) keeps it instead of getting `bg` pasted on top.
local function on_bg(style, bg)
  local s = {}
  if style then for k, v in pairs(style) do s[k] = v end end
  if bg and not s.bg and not s.hl_group then s.bg = bg end
  return s
end

local function text_width(runs)
  local w = 0
  for _, run in ipairs(runs) do w = w + smelt.text.width(run.text) end
  return w
end

--- Lay `items` out as a filled row. `opts` accepts `bg` (theme group that
--- fills empty space and backs items without their own background),
--- `separator` (text between separated items), `separator_style` and
--- `fill_selectable` (include the filler between items in drag-copy). An
--- item's `fill` overrides `bg` for the row.
function M.layout_strip(items, width, opts)
  opts = opts or {}
  local bg_group, row_fill = opts.bg, nil
  for _, item in ipairs(items) do
    if item.fill then bg_group, row_fill = item.fill, item.fill end
  end
  local sep = opts.separator or DEFAULT_SEPARATOR
  local sep_w = smelt.text.width(sep)

  local entries = fit(entries_of(items), width, function(es)
    local left = side_size(es, false, sep_w)
    local right, right_count = side_size(es, true, sep_w)
    return left + right + (right_count > 0 and side_gap(es) or 0)
  end)

  local function with_default_bg(style) return on_bg(style, bg_group) end
  local fill_style = row_fill and { hl_group = row_fill } or with_default_bg(nil)
  local sep_style = with_default_bg(opts.separator_style or { dim = true })
  local fill_selectable = opts.fill_selectable == true

  local left_runs, right_runs = {}, {}
  join_side(entries, false, left_runs, with_default_bg, fill_style, sep, sep_style, fill_selectable)
  join_side(entries, true, right_runs, with_default_bg, fill_style, sep, sep_style, fill_selectable)

  local right_start = math.max(width - text_width(right_runs), 0)

  -- What still overflows is clipped at the edge rather than dropped, and the
  -- cut always shows as `…`, even when it falls between two runs.
  local segs = {}
  local col = 0
  local function place(runs)
    local rest = text_width(runs)
    for _, run in ipairs(runs) do
      if col >= width then return end
      local w = smelt.text.width(run.text)
      rest = rest - w
      local room = width - col
      if w > room or (w == room and rest > 0) then
        local kept = w > room and smelt.text.fit(run.text, room, { suffix = "…" })
          or smelt.text.fit(run.text, room - 1, { suffix = "" }) .. "…"
        run = { text = kept, style = run.style, selectable = run.selectable }
        w = smelt.text.width(kept)
      end
      segs[#segs + 1] = run
      col = col + w
    end
  end
  place(left_runs)
  if col < right_start then
    segs[#segs + 1] = { text = string.rep(" ", right_start - col), style = fill_style, selectable = fill_selectable }
    col = right_start
  end
  place(right_runs)
  if col < width then
    segs[#segs + 1] = { text = string.rep(" ", width - col), style = fill_style, selectable = fill_selectable }
  end

  return segments_to_line(segs)
end

--- Lay `items` out as a rule with the left items set in from the left edge and
--- the right items in from the right. `opts` accepts `rule` (the cell repeated
--- as the line), `rule_style`, `bg` (theme group behind the whole row),
--- `separator` and `separator_style` (default: the rule style). A side with no
--- surviving item leaves a plain rule.
function M.layout_rule(items, width, opts)
  opts = opts or {}
  width = math.max(width or 0, 0)
  local rule = opts.rule or DEFAULT_RULE
  local rule_w = math.max(smelt.text.width(rule), 1)
  local sep = opts.separator or DEFAULT_SEPARATOR
  local sep_w = smelt.text.width(sep)

  local function with_bg(style) return on_bg(style, opts.bg) end
  local rule_style = with_bg(opts.rule_style or DEFAULT_RULE_STYLE)
  local sep_style = with_bg(opts.separator_style or opts.rule_style or DEFAULT_RULE_STYLE)
  local gap_style = with_bg(nil)
  -- Cells a side spends outside its items: a rule cell and a space on the
  -- outer edge, a space on the inner edge.
  local edge = rule_w + 2

  local function sizes(es)
    local left, left_count = side_size(es, false, sep_w)
    local right, right_count = side_size(es, true, sep_w)
    return left_count > 0 and left + edge or 0, right_count > 0 and right + edge or 0
  end

  local entries = fit(entries_of(items), width, function(es)
    local left, right = sizes(es)
    return left + RULE_MIN_CELLS + right
  end)

  -- Priority 0 items may still overflow: give up the left side, then the right.
  local function without(es, right)
    local kept = {}
    for _, e in ipairs(es) do
      if e.right ~= right then kept[#kept + 1] = e end
    end
    return kept
  end
  local left_w, right_w = sizes(entries)
  if left_w + right_w > width and left_w > 0 then
    entries = without(entries, false)
    left_w, right_w = sizes(entries)
  end
  if left_w + right_w > width and right_w > 0 then
    entries = without(entries, true)
    left_w, right_w = sizes(entries)
  end

  -- A wide rule cell that does not divide the run evenly ends in spaces.
  local function rule_run(cells)
    cells = math.max(cells, 0)
    local n = math.floor(cells / rule_w)
    return { text = string.rep(rule, n), style = rule_style, selectable = false },
      { text = string.rep(" ", cells - n * rule_w), style = gap_style, selectable = false }
  end

  local segs = {}
  local function push(...)
    for _, seg in ipairs({ ... }) do segs[#segs + 1] = seg end
  end
  if left_w > 0 then
    push(rule_run(rule_w))
    push({ text = " ", style = gap_style, selectable = false })
    join_side(entries, false, segs, with_bg, gap_style, sep, sep_style)
    push({ text = " ", style = gap_style, selectable = false })
  end
  push(rule_run(width - left_w - right_w))
  if right_w > 0 then
    push({ text = " ", style = gap_style, selectable = false })
    join_side(entries, true, segs, with_bg, gap_style, sep, sep_style)
    push({ text = " ", style = gap_style, selectable = false })
    push(rule_run(rule_w))
  end

  return segments_to_line(segs)
end

-- ── bars ────────────────────────────────────────────────────────────

-- Look options a bar takes from `configure`. Everything else on a bar is
-- engine state.
local LOOK = {
  layout = true,
  bg = true,
  separator = true,
  separator_style = true,
  rule = true,
  rule_style = true,
  fill_selectable = true,
  visible = true,
}
-- Look keys whose `false` is a value rather than "restore the default".
local BOOLEAN_LOOK = { fill_selectable = true, visible = true }

--- Look of a bar, set with `bar:configure(config)`. Absent keys keep their
--- current value; `false` resets a key to its default.
---@class smelt.bar.Config
---@field layout? "strip"|"rule" `"strip"` fills the row; `"rule"` draws a horizontal line with the items set into it.
---@field bg? string|false Theme group that fills the empty cells and backs items without their own background.
---@field separator? string|false Text between neighbouring `separated` items. Defaults to `" · "`.
---@field separator_style? smelt.buf.MarkOpts|false Style of the separator text.
---@field rule? string|false Cell repeated to draw a rule. Defaults to `"─"`.
---@field rule_style? smelt.buf.MarkOpts|false Style of the rule. Defaults to `{ fg = "SmeltSeparator" }`.
---@field fill_selectable? boolean Include the filler between items in drag-copy.
---@field visible? boolean `false` hides the bar: it takes no rows and its sources stay registered.

--- Options for `smelt.bar.new`. The look keys of `smelt.bar.Config` set the
--- defaults that `bar:configure({ key = false })` restores.
---@class smelt.bar.Opts
---@field name string Unique name; it labels errors and names the bar's window.
---@field layout? "strip"|"rule" Defaults to `"strip"`.
---@field bg? string Theme group behind the empty cells.
---@field separator? string Text between neighbouring `separated` items.
---@field separator_style? smelt.buf.MarkOpts Style of the separator text.
---@field rule? string Cell repeated to draw a rule.
---@field rule_style? smelt.buf.MarkOpts Style of the rule.
---@field fill_selectable? boolean Include the filler between items in drag-copy.
---@field visible? boolean Defaults to `true`.
---@field min_rows? integer Rows kept even when no source shows an item. Defaults to 1; 0 makes the bar collapse while empty.
---@field sources? { [1]: string, [2]: fun(ctx: smelt.bar.Context): smelt.bar.Item[]? }[] Built-in sources added in order; `bar:builtin(name)` returns them.
---@field win_opts? table Extra `smelt.win.new` options for the bar's window, such as `focus_on_click` or `resize`.

--- A bar: named sources whose items lay out in a strip or a rule. It owns a
--- window and its renderer; `bar:node()` places it in a layout.
---@class smelt.bar.Bar
---@field name string The name given to `smelt.bar.new`.
---@field win smelt.win.Win The window the bar renders into.
---@field add fun(self: smelt.bar.Bar, name: string, handler: fun(ctx: smelt.bar.Context): smelt.bar.Item[]?, opts?: { builtin?: boolean }): smelt.Reg Add a named source. `handler(ctx)` runs on every repaint (see `smelt.bar.Context`) and returns an item or a list of them; an error skips the source and shows a notification. Sources run in the order they were added. Adding an existing name replaces that source in place. `opts.builtin` also records it as the default that `bar:builtin(name)` returns, for features that ship a source. Returns a `Reg` that removes the source, unless a later `add` replaced it.
---@field remove fun(self: smelt.bar.Bar, name: string) Remove every source named `name`.
---@field clear fun(self: smelt.bar.Bar) Remove every source, built-in ones included.
---@field builtin fun(self: smelt.bar.Bar, name: string): (fun(ctx: smelt.bar.Context): smelt.bar.Item[]?)? The built-in source named `name`, so a replacement can fall back to it.
---@field configure fun(self: smelt.bar.Bar, config: smelt.bar.Config) Change the look. Absent keys stay; `false` restores a key's default.
---@field config fun(self: smelt.bar.Bar): smelt.bar.Config The current look.
---@field invalidate fun(self: smelt.bar.Bar) Repaint after state a source reads changes outside a built-in signal. Adding, replacing and removing sources repaint on their own.
---@field invalidate_after fun(self: smelt.bar.Bar, ms: integer) Repaint after `ms` milliseconds. An animated source asks for its next frame on every paint; the earliest pending request wins.
---@field rows fun(self: smelt.bar.Bar): integer Rows the bar wants: 0 while hidden, the view rows when a view replaces it, 1 for a rule, otherwise one per distinct item `row` and at least `min_rows`.
---@field highlight fun(self: smelt.bar.Bar, style: smelt.buf.MarkOpts?) Paint the rule and the separators with `style` until called with nil, for example while the bar is dragged. The look stays as configured.
---@field node fun(self: smelt.bar.Bar): smelt.ui.layout, integer The bar's layout node and rows, for layout composers.
---@field set_view fun(self: smelt.bar.Bar, node: smelt.ui.layout?, rows: integer?) Replace the bar with any layout node, such as a paint leaf, taking `rows` rows (default 1). `nil` restores the bar.

local Bar = {}
Bar.__index = Bar

-- Layout options for one row of `bar`.
local function look_of(bar)
  return {
    bg = bar.bg,
    separator = bar.separator,
    separator_style = bar.highlighted or bar.separator_style,
    rule = bar.rule,
    rule_style = bar.highlighted or bar.rule_style,
    fill_selectable = bar.fill_selectable,
  }
end

-- Display order of two `row` values: non-negative rows from the top, negative
-- rows from the bottom.
local function row_before(a, b)
  if (a < 0) ~= (b < 0) then return b < 0 end
  return a < b
end

-- The distinct `row` values of the visible `items`, in display order.
local function rows_of(items)
  local seen, rows = {}, {}
  for _, entry in ipairs(entries_of(items)) do
    local r = entry.item.row or 1
    if not seen[r] then
      seen[r] = true
      rows[#rows + 1] = r
    end
  end
  table.sort(rows, row_before)
  return rows
end

-- Every source's items in registration order, for a bar `width` cells wide.
-- With `rows`, each source learns how many rows the earlier ones left. A
-- source may return a single item instead of a list. A failing source is
-- skipped and notified; the message log folds the repeats.
local function collect(bar, width, rows)
  local items = {}
  for _, src in ipairs(bar.sources) do
    local ctx = { width = width }
    if rows then ctx.rows = math.max(rows - #rows_of(items), 0) end
    local ok, result = pcall(src.handler, ctx)
    if not ok then
      smelt.notify.error(bar.name .. " source `" .. src.name .. "`: " .. tostring(result))
    elseif type(result) == "table" then
      if result.text or result.runs then result = { result } end
      for _, item in ipairs(result) do
        items[#items + 1] = item
      end
    end
  end
  return items
end

-- The items for `width`, collected again when it changes. Without a width, the
-- last one laid out, or the terminal's before the first layout. With `rows`,
-- the items for a bar that gets fewer rows than it wants, cached apart so the
-- full set that `rows()` measures stays valid.
local function items_of(bar, width, rows)
  width = width or bar.items_width or smelt.terminal.size().width
  if not bar.items or bar.items_width ~= width then
    bar.items, bar.items_width, bar.folded = collect(bar, width), width, nil
  end
  if not rows then return bar.items end
  if not bar.folded or bar.folded.rows ~= rows then
    bar.folded = { rows = rows, items = collect(bar, width, rows) }
  end
  return bar.folded.items
end

function Bar:rows()
  if not self.visible then return 0 end
  if self.view then return self.view.rows end
  if self.layout == "rule" then return 1 end
  return math.max(self.min_rows, #rows_of(items_of(self, self.items_width)))
end

function Bar:highlight(style)
  self.highlighted = style
  if self.win then self.win:invalidate_renderer() end
end

-- The layout composer calls this, so the rows it returns are the rows
-- `invalidate` compares against.
function Bar:node()
  self.shown_rows = self:rows()
  if self.view then return self.view.node, self.shown_rows end
  return smelt.ui.layout.leaf(self.win), self.shown_rows
end

function Bar:set_view(node, rows)
  if node == nil then
    self.view = nil
  else
    self.view = { node = node, rows = rows or 1 }
  end
  smelt.ui.layout.invalidate()
end

function Bar:invalidate()
  self.items, self.folded = nil, nil
  if self.win then self.win:invalidate_renderer() end
  -- The height follows the items, so relayout in the same frame when the row
  -- count changes.
  if self.shown_rows and self:rows() ~= self.shown_rows then
    smelt.ui.layout.invalidate()
  end
end

-- The earliest pending request wins, so sources animating at different rates
-- share one timer.
function Bar:invalidate_after(ms)
  local now = smelt.time.monotonic_ms()
  if self.repaint and now <= self.repaint_at and self.repaint_at <= now + ms then return end
  if self.repaint then self.repaint:remove() end
  self.repaint_at = now + ms
  self.repaint = smelt.timer.set(ms, function()
    self.repaint = nil
    self:invalidate()
  end)
end

function Bar:add(name, handler, opts)
  if type(name) ~= "string" or name == "" then
    error(self.name .. ":add: name must be a non-empty string", 2)
  end
  if type(handler) ~= "function" then
    error(self.name .. ":add: handler must be a function", 2)
  end
  local entry = { name = name, handler = handler }
  local replaced = false
  for i, src in ipairs(self.sources) do
    if src.name == name then
      self.sources[i] = entry
      replaced = true
      break
    end
  end
  if not replaced then self.sources[#self.sources + 1] = entry end
  if opts and opts.builtin then self.builtins[name] = handler end
  self:invalidate()
  return smelt.reg.new(function()
    for i, src in ipairs(self.sources) do
      if src == entry then
        table.remove(self.sources, i)
        self:invalidate()
        return
      end
    end
  end)
end

function Bar:remove(name)
  for i = #self.sources, 1, -1 do
    if self.sources[i].name == name then table.remove(self.sources, i) end
  end
  self:invalidate()
end

function Bar:clear()
  self.sources = {}
  self:invalidate()
end

function Bar:builtin(name)
  return self.builtins[name]
end

function Bar:configure(config)
  if type(config) ~= "table" then
    error(self.name .. ":configure: config must be a table", 2)
  end
  for key, value in pairs(config) do
    if not LOOK[key] then
      error(self.name .. ":configure: unknown key `" .. tostring(key) .. "`", 2)
    end
    if value == false and not BOOLEAN_LOOK[key] then value = self.defaults[key] end
    if key == "layout" and value ~= "strip" and value ~= "rule" then
      error(self.name .. ':configure: layout must be "strip" or "rule"', 2)
    end
    self[key] = value
  end
  self:invalidate()
  smelt.ui.layout.invalidate()
end

function Bar:config()
  local out = {}
  for key in pairs(LOOK) do out[key] = self[key] end
  return out
end

-- `bar` laid out for `width` cells, `rows` rows. A rule takes the first row
-- and leaves the rest blank. A strip stacks its distinct item rows in order;
-- with fewer rows than it wants, its sources learn how many they have.
local function layout_rows(bar, width, rows)
  local look = look_of(bar)
  local out = {}
  if bar.layout == "rule" then
    local items = items_of(bar, width)
    for r = 1, rows do
      out[r] = r == 1 and M.layout_rule(items, width, look) or M.layout_strip({}, width, look)
    end
    return out
  end
  local items = items_of(bar, width)
  if #rows_of(items) > rows then items = items_of(bar, width, rows) end
  local slot = {}
  for i, r in ipairs(rows_of(items)) do slot[r] = i end
  local by_slot = {}
  for _, item in ipairs(items) do
    local i = slot[item.row or 1]
    if i then
      by_slot[i] = by_slot[i] or {}
      table.insert(by_slot[i], item)
    end
  end
  for i = 1, rows do
    out[i] = M.layout_strip(by_slot[i] or {}, width, look)
  end
  return out
end

--- Create a bar (see `smelt.bar.Opts`).
function M.new(opts)
  if type(opts) ~= "table" or type(opts.name) ~= "string" or opts.name == "" then
    error("smelt.bar.new: opts.name must be a non-empty string", 2)
  end
  local defaults = {}
  for key in pairs(LOOK) do defaults[key] = opts[key] end
  defaults.layout = defaults.layout or "strip"
  if defaults.visible == nil then defaults.visible = true end
  local bar = setmetatable({
    name = opts.name,
    min_rows = opts.min_rows or 1,
    defaults = defaults,
    sources = {}, -- ordered { name, handler } pairs
    builtins = {},
    items = nil, -- collected on demand, dropped on invalidate
    folded = nil, -- items for fewer rows than wanted, dropped on invalidate
    shown_rows = nil, -- rows the layout last reserved
  }, Bar)
  for key, value in pairs(defaults) do bar[key] = value end

  local win_opts = { name = opts.name, scrollbar = false, surface = "selectable_text" }
  for key, value in pairs(opts.win_opts or {}) do win_opts[key] = value end
  bar.win = smelt.win.new(smelt.buf.new({ name = opts.name }), win_opts)
  if bar.win then
    local ns = smelt.ns(opts.name)
    bar.win:set_renderer(function(win)
      local buf = win:buf()
      if not buf then return end
      local height = (win:rect() or {}).height or bar:rows()
      write_rows(buf, layout_rows(bar, win:content_width() or 80, height), ns)
    end)
  end

  for _, source in ipairs(opts.sources or {}) do
    bar:add(source[1], source[2], { builtin = true })
  end
  return bar
end

smelt.bar = smelt.bar or {}

--- Create a bar that owns a window and lays out items from named sources (see
--- `smelt.bar.Opts`). Place it in a layout with `bar:node()`.
---@tier ui_host
---@type fun(opts: smelt.bar.Opts): smelt.bar.Bar
function smelt.bar.new(opts)
  return M.new(opts)
end

return M
