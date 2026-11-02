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
-- `style` accepts `smelt.Style|string`, shared with buffer marks and Paint.
-- `fg` / `bg` take any color form, including a theme group name.
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
---@field runs? smelt.Span[] Consecutive styled pieces of one item; they drop together and are never truncated.
---@field style? smelt.Style|string Highlight applied to the text (`fg`, `bg`, `hl_group`, `bold`, `dim`, `italic`).
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
---@field width integer Content width for sources; a width budget during view construction. Renderers receive their actual width.
---@field rows? integer Rows left for this source while the layout gives the bar fewer rows than it wants: the bar's rows minus the rows earlier sources took. Nil when every row fits.
---@field invalidate_after fun(ms: integer) Schedule the destination's next repaint. Use this rather than capturing a particular bar.
---@field state table Host state. Transcript bars expose { label, target?, view }; other bars default to an empty table. A view factory's context stays live until its view is released.

--- Portable content with its update dependencies. Wrappers retain `signals` and call `render(ctx)`.
---@class smelt.bar.Source
---@field render fun(ctx: smelt.bar.Context): smelt.bar.Item|smelt.bar.Item[]|nil Render for the destination's size and scheduler.
---@field signals? (string|{ [1]: string, [2]: fun(value: any): any })[] Signals that invalidate the destination. A pair adds a display-value projection, so insignificant changes do not repaint.

--- Styled text for buffer lines, titles and bar runs.
---@class smelt.Span
---@field text string Text to show.
---@field style? smelt.Style|string Highlight applied to the text.
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
        local opts = { style = hl.style, end_col = hl.bytes_end }
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
  if type(style) == "table" and style.bg ~= nil then return true end
  if type(style) == "string" or (style and style.hl_group) then
    if entry.block_bg == nil then entry.block_bg = smelt.style.resolve(style).bg ~= nil end
    return entry.block_bg
  end
  return false
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

-- Keep explicit backgrounds, including false. Foreground-only groups still
-- inherit the bar background; their reference must survive theme changes.
local function on_bg(style, bg)
  if bg == nil then return style or {} end
  if style == nil then return { bg = bg } end
  if type(style) == "table" and style.bg ~= nil then return style end
  local group = type(style) == "string" and style or style.hl_group
  if group and smelt.style.resolve(group).bg ~= nil then return style end
  -- Marks validate the completed style; inheritance only copies references.
  local merged = { bg = bg }
  if type(style) == "string" then merged.hl_group = style
  else for key, value in pairs(style) do merged[key] = value end end
  return merged
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
  style = true,
  on_press = true,
}

--- Look of a bar, set with `bar:configure(config)`. Absent keys keep their
--- current value; `bar:reset(key)` restores constructor defaults.
---@class smelt.bar.Config
---@field layout? "strip"|"rule" `"strip"` fills the row; `"rule"` draws a horizontal line with the items set into it.
---@field bg? string|false Theme group that fills the empty cells and backs items without their own background.
---@field separator? string Text between neighbouring `separated` items. Defaults to `" · "`.
---@field separator_style? smelt.Style|string Style of the separator text.
---@field rule? string Cell repeated to draw a rule. Defaults to `"─"`.
---@field rule_style? smelt.Style|string Style of the rule. Defaults to `{ fg = "SmeltSeparator" }`.
---@field fill_selectable? boolean Include the filler between items in drag-copy.
---@field visible? boolean `false` hides the bar: it takes no rows and its sources stay registered.
---@field style? smelt.Style|string Default style for items without their own style.
---@field on_press? false|fun(ctx: smelt.bar.Context, event: table) Left-button action shared by every leaf; false disables it.

--- Options for `smelt.bar.new`. The look keys of `smelt.bar.Config` set the
--- defaults that `bar:reset(key)` restores.
---@class smelt.bar.Opts
---@field name string Unique name; it labels errors and names the bar's window.
---@field layout? "strip"|"rule" Defaults to `"strip"`.
---@field bg? string Theme group behind the empty cells.
---@field separator? string Text between neighbouring `separated` items.
---@field separator_style? smelt.Style|string Style of the separator text.
---@field rule? string Cell repeated to draw a rule.
---@field rule_style? smelt.Style|string Style of the rule.
---@field fill_selectable? boolean Include the filler between items in drag-copy.
---@field visible? boolean Defaults to `true`.
---@field style? smelt.Style|string Default style for items without their own style.
---@field on_press? false|fun(ctx: smelt.bar.Context, event: table) Left-button action for the bar's leaves.
---@field min_rows? integer Rows kept even when no source shows an item. Defaults to 1; 0 makes the bar collapse while empty.
---@field sources? { [1]: string, [2]: smelt.bar.Source|(fun(ctx: smelt.bar.Context): (smelt.bar.Item|smelt.bar.Item[]|nil)) }[] Built-in sources added in order; `bar:builtin(name)` returns them.
---@field win_opts? table Extra window options, such as focus_on_click or padding.

--- A bar: named sources whose items lay out in a strip or a rule. It owns a
--- window and its renderer; `bar:node()` places it in a layout.
---@class smelt.bar.Bar
---@field name string The name given to `smelt.bar.new`.
---@field win smelt.win.Win? Source-rendering window. Overlay bars have none while unmounted or using a replacement view.
---@field add fun(self: smelt.bar.Bar, name: string, handler: smelt.bar.Source|(fun(ctx: smelt.bar.Context): (smelt.bar.Item|smelt.bar.Item[]|nil)), opts?: { row?: integer, align_right?: boolean, priority?: integer }): smelt.Reg Add or replace a named source. Its signals update this bar; removal removes subscriptions. Placement overrides do not modify returned items. Errors skip the source and notify. The Reg removes only this registration, not a later replacement.
---@field add_builtin fun(self: smelt.bar.Bar, name: string, source: smelt.bar.Source|(fun(ctx: smelt.bar.Context): (smelt.bar.Item|smelt.bar.Item[]|nil))): smelt.Reg Add a default source retrievable with builtin(name). Prefer constructor sources for a new bar.
---@field remove fun(self: smelt.bar.Bar, name: string) Remove every source named `name`.
---@field clear fun(self: smelt.bar.Bar) Remove every source, built-in ones included.
---@field builtin fun(self: smelt.bar.Bar, name: string): smelt.bar.Source? The built-in source, including its signals, so it can be moved or wrapped without losing updates.
---@field configure fun(self: smelt.bar.Bar, config: smelt.bar.Config) Validate and change the look atomically. Absent keys stay unchanged.
---@field reset fun(self: smelt.bar.Bar, key?: string) Restore one constructor default, or every look option when key is omitted.
---@field config fun(self: smelt.bar.Bar): smelt.bar.Config The current look.
---@field invalidate fun(self: smelt.bar.Bar) Repaint after state a source reads changes outside a built-in signal. Adding, replacing and removing sources repaint on their own.
---@field invalidate_after fun(self: smelt.bar.Bar, ms: integer) Repaint after `ms` milliseconds. An animated source asks for its next frame on every paint; the earliest pending request wins.
---@field rows fun(self: smelt.bar.Bar): integer Rows the bar wants: 0 while hidden, the view rows when a view replaces it, 1 for a rule, otherwise one per distinct item `row` and at least `min_rows`.
---@field highlight fun(self: smelt.bar.Bar, style: smelt.Style|string?) Paint the rule and the separators with `style` until called with nil, for example while the bar is dragged. The look stays as configured.
---@field node fun(self: smelt.bar.Bar): smelt.ui.layout, integer The bar's layout node and rows, for layout composers.
---@field set_view fun(self: smelt.bar.Bar, factory: (fun(ctx: smelt.bar.Context): smelt.ui.layout)?) Build a measured replacement view with fresh leaves. The bar releases its resources and registrations on replacement or host teardown; failed construction preserves the current view. Names inside a factory are local to that view. Give Paint leaves a measure. nil restores source rendering.

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
    local ctx = { width = width, state = bar.context.state, invalidate_after = bar.schedule }
    if rows then ctx.rows = math.max(rows - #rows_of(items), 0) end
    local ok, result = pcall(src.handler, ctx)
    if not ok then
      smelt.notify.error(bar.name .. " source `" .. src.name .. "`: " .. tostring(result))
    elseif type(result) == "table" then
      if result.text or result.runs then result = { result } end
      for _, item in ipairs(result) do
        if src.presentation or (bar.style and item.style == nil) then
          local copy = {}
          for key, value in pairs(item) do copy[key] = value end
          for key, value in pairs(src.presentation or {}) do copy[key] = value end
          if copy.style == nil then copy.style = bar.style end
          item = copy
        end
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
    bar.item_rows = rows_of(bar.items)
  end
  if not rows then return bar.items end
  if not bar.folded or bar.folded.rows ~= rows then
    bar.folded = { rows = rows, items = collect(bar, width, rows) }
  end
  return bar.folded.items
end

function Bar:rows()
  if not self.visible or (self.hosted and not self.host_opts) then return 0 end
  local view = self.presentation
  if view and view.owner then
    local size = smelt.terminal.size()
    return select(2, smelt.ui.layout.size(view.node, self.hosted and self.context.width or (self.items_width or size.width), size.height))
  end
  if self.layout == "rule" then return 1 end
  items_of(self, self.items_width)
  return math.max(self.min_rows, #self.item_rows)
end

function Bar:highlight(style)
  self.highlighted = style
  if self.win then self.win:invalidate() end
end

-- The layout composer calls this, so the rows it returns are the rows
-- `invalidate` compares against.
function Bar:node()
  local view = self.presentation
  if not view then return smelt.ui.layout.vbox({}), 0 end
  self.shown_rows = self:rows()
  if not view.owner and not self.hosted then view.measure:set(0, self.shown_rows) end
  return view.node, self.shown_rows
end

local function clear_press(view)
  if not view.press_regs then return end
  for _, reg in ipairs(view.press_regs) do reg:remove() end
  view.press_regs = nil
end

local function release_view(bar, view)
  if not view then return end
  clear_press(view)
  if view == bar.source_view then return end
  if view.owner then view.owner:remove()
  elseif view.win then
    view.win:set_renderer(nil)
    view.win:close()
  end
end

local function build_view(bar, factory)
  local leaves
  local node, owner = __smelt_internal.ui.layout.build_view(function(ctx)
    local node = factory(ctx)
    leaves = smelt.ui.layout.leaves(node)
    return node
  end, bar.context, bar.presentation and bar.presentation.node)
  return { node = node, owner = owner, leaves = leaves }
end

function Bar:_mount_view(view)
  local opts = {}
  for key, value in pairs(self.host_opts) do opts[key] = value end
  opts.layout = view.node
  return smelt.overlay.new(opts)
end

function Bar:set_view(factory)
  assert(factory == nil or type(factory) == "function", "bar.set_view: expected factory or nil")
  if factory == nil and self.view_factory == nil then self:invalidate(); return end
  if self.hosted and (not self.host_opts or not self.visible) then
    self.view_factory = factory
  else
    if not self.hosted then self.context.width = self.items_width or smelt.terminal.size().width end
    local candidate = factory and build_view(self, factory) or self.source_view or self:_default_view()
    local ok, overlay = pcall(function()
      self:_wire_press(candidate)
      if self.hosted then return self:_mount_view(candidate) end
    end)
    if not ok then release_view(self, candidate); error(overlay, 0) end
    local previous = self.presentation
    self.presentation, self.view_factory = candidate, factory
    if self.hosted then self.overlay, self.win = overlay, candidate.win end
    release_view(self, previous)
  end
  self:invalidate()
  smelt.ui.layout.invalidate()
end

-- Overlay hosts only supply navigation state and placement. Presentation and
-- resource lifetime use the same bar implementation as root-layout bars.
function Bar:_hide()
  if self.overlay then self.overlay:close() end
  self.overlay = nil
  release_view(self, self.presentation)
  self.presentation, self.win = nil, nil
  if self.repaint then self.repaint:remove(); self.repaint = nil end
end

function Bar:_remount()
  if not self.host_opts then return end
  if not self.visible then self:_hide(); return end
  local candidate = self.presentation
  if not candidate then
    candidate = self.view_factory and build_view(self, self.view_factory) or self:_default_view()
    self:_wire_press(candidate)
  end
  local ok, overlay = pcall(self._mount_view, self, candidate)
  if not ok then
    if candidate ~= self.presentation then release_view(self, candidate) end
    error(overlay, 0)
  end
  self.presentation, self.overlay, self.win = candidate, overlay, candidate.win
end

function Bar:_present(state, width, opts)
  local resized = self.context.width ~= width
  self.context.state, self.context.width = state, width
  self.host_opts = opts
  local label = self.builtins.label
  local label_only = label and #self.sources == 1 and self.sources[1].handler == label.render
  local changed = resized or self.last_label ~= state.label or not label_only
  if changed then self.items, self.folded = nil, nil end
  if resized and self.overlay then self:_hide() end
  if not self.overlay and self.visible then self:_remount() end
  local view = self.presentation
  if view and view.owner then
    for _, leaf in ipairs(view.leaves) do leaf:invalidate() end
  elseif self.overlay and changed then
    self.last_label = state.label
    self:invalidate()
  end
end

function Bar:_dismiss()
  self.host_opts = nil
  self:_hide()
end

function Bar:invalidate()
  self.items, self.folded = nil, nil
  local view = self.presentation
  if view then
    if view.owner then
      for _, leaf in ipairs(view.leaves) do leaf:invalidate() end
    else
      view.win:invalidate()
      if self.hosted then self:_measure_default(view) end
    end
  end
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

local function projected_change(project, value, previous)
  return project(value) ~= project(previous)
end

local function detach(bar, source)
  for _, dependency in ipairs(source.dependencies) do
    local group = dependency.group
    group.members[dependency] = nil
    if next(group.members) == nil then
      group.reg:remove()
      bar.signal_groups[dependency.key] = nil
    end
  end
end

function Bar:add(name, handler, opts)
  if type(name) ~= "string" or name == "" then
    error(self.name .. ":add: name must be a non-empty string", 2)
  end
  local render = type(handler) == "table" and handler.render or handler
  if type(render) ~= "function" then
    error(self.name .. ":add: source must be a function or { render, signals }", 2)
  end
  local entry = { name = name, handler = render, dependencies = {} }
  for key, value in pairs(opts or {}) do
    if key == "row" or key == "priority" then
      assert(math.type(value) == "integer", "bar source " .. key .. ": expected integer")
    elseif key == "align_right" then
      assert(type(value) == "boolean", "bar source " .. key .. ": expected boolean")
    else error("bar source: unknown option " .. tostring(key), 2) end
    entry.presentation = entry.presentation or {}
    entry.presentation[key] = value
  end
  if type(handler) == "table" then
    local ok, err = pcall(function()
      for _, signal in ipairs(handler.signals or {}) do
        local key, project = signal, nil
        if type(signal) == "table" then key, project = signal[1], signal[2] end
        assert(project == nil or type(project) == "function", "bar source: signal projection must be a function")
        local group = self.signal_groups[key]
        if not group then
          group = { members = {} }
          group.reg = __smelt_internal.signal.subscribe_shared(key, function(value, previous)
            for dependency in pairs(group.members) do
              if not dependency.project then self:invalidate(); return end
              local ok, changed = pcall(projected_change, dependency.project, value, previous)
              if not ok then
                smelt.notify.error(self.name .. " signal `" .. key .. "`: " .. tostring(changed))
              elseif changed then
                self:invalidate()
                return
              end
            end
          end)
          self.signal_groups[key] = group
        end
        local dependency = { key = key, project = project, group = group }
        group.members[dependency] = true
        entry.dependencies[#entry.dependencies + 1] = dependency
      end
    end)
    if not ok then detach(self, entry); error(err, 2) end
  end
  local replaced = false
  for i, src in ipairs(self.sources) do
    if src.name == name then
      detach(self, src)
      self.sources[i] = entry
      replaced = true
      break
    end
  end
  if not replaced then self.sources[#self.sources + 1] = entry end
  self:invalidate()
  return smelt.reg.new(function()
    for i, src in ipairs(self.sources) do
      if src == entry then
        detach(self, src)
        table.remove(self.sources, i)
        self:invalidate()
        return
      end
    end
  end)
end

function Bar:add_builtin(name, source)
  local reg = self:add(name, source)
  self.builtins[name] = type(source) == "table" and source or { render = source }
  return reg
end

function Bar:remove(name)
  for i = #self.sources, 1, -1 do
    if self.sources[i].name == name then
      detach(self, self.sources[i])
      table.remove(self.sources, i)
    end
  end
  self:invalidate()
end

function Bar:clear()
  for _, src in ipairs(self.sources) do detach(self, src) end
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
    if key == "rule_style" or key == "separator_style" or key == "style" then smelt.style.resolve(value) end
    if key == "on_press" and value ~= false and type(value) ~= "function" then error("bar.on_press: expected function or false", 2) end
    if key == "bg" then smelt.theme.rgb(value, "bg") end
    if key == "layout" and value ~= "strip" and value ~= "rule" then
      error(self.name .. ':configure: layout must be "strip" or "rule"', 2)
    elseif (key == "separator" or key == "rule") and type(value) ~= "string" then
      error(self.name .. ":configure: " .. key .. " must be a string", 2)
    elseif (key == "visible" or key == "fill_selectable") and type(value) ~= "boolean" then
      error(self.name .. ":configure: " .. key .. " must be a boolean", 2)
    end
  end
  for key, value in pairs(config) do self[key] = value end
  if config.on_press ~= nil then self:_wire_press() end
  if self.hosted then self:_remount() end
  self:invalidate()
  smelt.ui.layout.invalidate()
end

function Bar:reset(key)
  if key ~= nil and not LOOK[key] then error(self.name .. ":reset: unknown key " .. tostring(key), 2) end
  for name in pairs(LOOK) do
    if key == nil or name == key then self[name] = self.defaults[name] end
  end
  if key == nil or key == "on_press" then self:_wire_press() end
  if key == nil then self:set_view(nil) end
  if self.hosted then self:_remount() end
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
  local item_rows = bar.item_rows
  if #item_rows > rows then
    items = items_of(bar, width, rows)
    item_rows = rows_of(items)
  end
  local slot = {}
  for i, r in ipairs(item_rows) do slot[r] = i end
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

function Bar:_natural_width()
  if self.layout == "rule" then return self.context.width end
  local rows, width = {}, 0
  for _, item in ipairs(items_of(self, self.context.width)) do
    local row = item.row or 1
    rows[row] = rows[row] or {}
    rows[row][#rows[row] + 1] = item
  end
  local separator = smelt.text.width(self.separator or DEFAULT_SEPARATOR)
  for _, items in pairs(rows) do
    local entries = entries_of(items)
    local left = side_size(entries, false, separator)
    local right, count = side_size(entries, true, separator)
    width = math.max(width, left + right + (count > 0 and side_gap(entries) or 0))
  end
  return math.min(self.context.width, width)
end

-- Fit sources get their actual width before height is measured, so responsive
-- rows are correct on the first frame, including a newly opened overlay.
function Bar:_measure_default(view)
  local width = self:_natural_width()
  items_of(self, width)
  local rows = self.layout == "rule" and 1 or math.max(self.min_rows, #self.item_rows)
  view.measure:set(width, rows)
end

function Bar:_wire_press(view)
  view = view or self.presentation
  if not view then return end
  clear_press(view)
  if not self.on_press then return end
  view.press_regs = {}
  for _, leaf in ipairs(view.leaves) do
    view.press_regs[#view.press_regs + 1] = leaf:on("press", function(ev)
      if self.on_press and (ev.button == nil or ev.button == "left") then self.on_press(self.context, ev) end
    end)
  end
end

function Bar:_default_view()
  local win_opts = { name = self.name .. (self.hosted and ".win" or ""), scrollbar = false, surface = "selectable_text" }
  for key, value in pairs(self.win_opts) do win_opts[key] = value end
  local buf = smelt.buf.new({ name = self.hosted and self.name .. ".buf" or self.name })
  local win = smelt.win.new(buf, win_opts)
  if not win then return nil end
  local measure = smelt.ui.layout.measure(0, self.min_rows)
  local view = { win = win, measure = measure, leaves = { win }, node = smelt.ui.layout.leaf(win, { measure = measure }) }
  if self.hosted then self:_measure_default(view) end
  local ns = smelt.ns(self.name)
  win:set_renderer(function()
    local height = (win:rect() or {}).height or self:rows()
    write_rows(buf, layout_rows(self, win:content_width() or self.context.width, height), ns)
    if self.shown_rows and self:rows() ~= self.shown_rows then smelt.ui.layout.invalidate() end
  end)
  return view
end

local function new_bar(opts, hosted)
  if type(opts) ~= "table" or type(opts.name) ~= "string" or opts.name == "" then
    error("smelt.bar.new: opts.name must be a non-empty string", 2)
  end
  local defaults = {}
  for key in pairs(LOOK) do defaults[key] = opts[key] end
  defaults.layout = defaults.layout or "strip"
  if defaults.visible == nil then defaults.visible = true end
  local bar = setmetatable({
    name = opts.name,
    hosted = hosted,
    win_opts = opts.win_opts or {},
    context = { state = {}, width = smelt.terminal.size().width },
    min_rows = opts.min_rows or 1,
    defaults = defaults,
    sources = {}, -- ordered { name, handler } pairs
    signal_groups = {},
    builtins = {},
    items = nil, -- collected on demand, dropped on invalidate
    folded = nil, -- items for fewer rows than wanted, dropped on invalidate
    shown_rows = nil, -- rows the layout last reserved
  }, Bar)
  for key, value in pairs(defaults) do bar[key] = value end
  bar.schedule = function(ms) bar:invalidate_after(ms) end

  bar.context.invalidate_after = bar.schedule
  if not hosted then
    bar.source_view = bar:_default_view()
    bar.presentation = bar.source_view
    bar.win = bar.source_view and bar.source_view.win
    bar:_wire_press()
  end
  for _, source in ipairs(opts.sources or {}) do
    bar:add_builtin(source[1], source[2])
  end
  return bar
end

function M.new(opts) return new_bar(opts, false) end
function M.overlay(opts) return new_bar(opts, true) end

smelt.bar = smelt.bar or {}

--- Create a bar that owns a window and lays out items from named sources (see
--- `smelt.bar.Opts`). Place it in a layout with `bar:node()`.
---@tier ui_host
---@type fun(opts: smelt.bar.Opts): smelt.bar.Bar
function smelt.bar.new(opts)
  return M.new(opts)
end

return M
