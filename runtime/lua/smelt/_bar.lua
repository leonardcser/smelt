-- Bar engine shared by the statusline and the prompt top and bottom bars.
--
-- A bar is a registry of named sources. A source is a function returning
-- items; a repaint calls every source in order, then lays the items out for
-- the bar's width. Two layouts share the item shape, the priority-drop fitter
-- and the spacing rules:
--
--   * "strip": a filled row with the left items and the right items packed
--     against opposite edges (statusline).
--   * "rule":  a horizontal rule with the left items and the right items set
--     into it (prompt top and bottom bars).
--
-- Internal - plugins reach bars through `smelt.statusline` and
-- `smelt.prompt_bar.top` / `.bottom`, which wrap `add`, `remove` and
-- `invalidate` below.
--
-- `style` is a subset of `smelt.buf.MarkOpts` (`fg`, `bg`, `hl_group`, `bold`,
-- `dim`, `italic`). `fg` / `bg` accept a theme group name (string) or a direct
-- `{ r, g, b }` triple. The same table is forwarded verbatim to `buf:mark`.
--
-- A laid-out row is `{ text, highlights }` where each highlight carries
-- `bytes_start` / `bytes_end` (byte offsets, the unit `buf:mark` expects), the
-- forwarded `style`, and optional `selectable` metadata. `write_rows(buf, rows,
-- ns)` projects a list of rows into a buffer.

local M = {}

local DASH = "\u{2500}"
local SEP = " \u{00b7} "
local SEP_WIDTH = 3
-- Cells a rule side spends outside its items: the dash and the space on the
-- left, the space and the dash on the right.
local RULE_EDGE = 3
local RULE_MIN_DASHES = 4
local DEFAULT_RULE_STYLE = { fg = "SmeltSeparator" }

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

--- A styled piece of a `runs` item.
---@class smelt.bar.Run
---@field text string Text to show.
---@field style? smelt.buf.MarkOpts Highlight applied to the text.
---@field selectable? boolean Include the text in drag-copy. Defaults to the item's setting.

-- ── rows ────────────────────────────────────────────────────────────

function M.segments_to_line(segs)
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

function M.truncate_right_padded(text, width, opts)
  opts = opts or {}
  width = math.max(width or 0, 0)
  local pad = opts.pad or 2
  local suffix = opts.suffix or "…"
  local pad_w = math.min(math.max(pad, 0), width)
  local body_w = math.max(0, width - pad_w)
  local body = smelt.text.truncate_cells(text or "", body_w, { suffix = suffix })
  return body .. string.rep(" ", pad_w), #body
end

function M.write_rows(buf, rows, ns)
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
local function side_size(entries, right)
  local w, count, sep_seen, prev = 0, 0, false, nil
  for _, e in ipairs(entries) do
    if e.right == right then
      if prev then
        if e.item.separated and sep_seen then
          w = w + SEP_WIDTH
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
-- and a `·` between consecutive separated items.
local function join_side(entries, right, out, style_of, gap_style, sep_style)
  local sep_seen, prev = false, nil
  for _, e in ipairs(entries) do
    if e.right == right then
      if prev then
        if e.item.separated and sep_seen then
          out[#out + 1] = { text = SEP, style = sep_style, selectable = false }
        elseif gap_between(prev, e) > 0 then
          out[#out + 1] = { text = " ", style = gap_style, selectable = false }
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

local function text_width(runs)
  local w = 0
  for _, run in ipairs(runs) do w = w + smelt.text.width(run.text) end
  return w
end

--- Lay `items` out as a filled row. `opts` accepts `bg_group` (theme group
--- that fills empty space and backs items without their own background) and
--- `sep_group` (theme group of the separator dots).
function M.layout_strip(items, width, opts)
  opts = opts or {}
  local bg_group = opts.bg_group

  local entries = fit(entries_of(items), width, function(es)
    local left = side_size(es, false)
    local right, right_count = side_size(es, true)
    return left + right + (right_count > 0 and 1 or 0)
  end)

  -- `hl_group` carries its own bg, so an item that picked one (a mode pill)
  -- keeps it instead of getting the fill color pasted on top.
  local function with_default_bg(style)
    local s = {}
    if style then for k, v in pairs(style) do s[k] = v end end
    if bg_group and not s.bg and not s.hl_group then s.bg = bg_group end
    return s
  end
  local fill_style = with_default_bg(nil)
  local sep_style = with_default_bg({ fg = opts.sep_group, dim = true })

  local left_runs, right_runs = {}, {}
  join_side(entries, false, left_runs, with_default_bg, fill_style, sep_style)
  join_side(entries, true, right_runs, with_default_bg, fill_style, sep_style)

  local right_start = math.max(width - text_width(right_runs), 0)

  local segs = {}
  local col = 0
  for _, run in ipairs(left_runs) do
    local w = smelt.text.width(run.text)
    if col + w > width then break end
    segs[#segs + 1] = run
    col = col + w
  end
  if col < right_start then
    segs[#segs + 1] = { text = string.rep(" ", right_start - col), style = fill_style, selectable = false }
    col = right_start
  end
  for _, run in ipairs(right_runs) do
    local w = smelt.text.width(run.text)
    if col + w > width then break end
    segs[#segs + 1] = run
    col = col + w
  end
  if col < width then
    segs[#segs + 1] = { text = string.rep(" ", width - col), style = fill_style, selectable = false }
  end

  return M.segments_to_line(segs)
end

--- Lay `items` out as a rule with the left items set in from the left edge and
--- the right items in from the right. `opts.bar_style` styles the rule and its
--- separators. A side with no surviving item leaves a plain rule.
function M.layout_rule(items, width, opts)
  opts = opts or {}
  width = math.max(width or 0, 0)
  local bar_style = opts.bar_style or DEFAULT_RULE_STYLE

  local function sizes(es)
    local left, left_count = side_size(es, false)
    local right, right_count = side_size(es, true)
    return left_count > 0 and left + RULE_EDGE or 0, right_count > 0 and right + RULE_EDGE or 0
  end

  local entries = fit(entries_of(items), width, function(es)
    local left, right = sizes(es)
    return left + RULE_MIN_DASHES + right
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

  local function plain(style) return style or {} end
  local segs = {}
  if left_w > 0 then
    segs[#segs + 1] = { text = DASH, style = bar_style, selectable = false }
    segs[#segs + 1] = { text = " ", style = {}, selectable = false }
    join_side(entries, false, segs, plain, {}, bar_style)
    segs[#segs + 1] = { text = " ", style = {}, selectable = false }
  end
  segs[#segs + 1] = {
    text = string.rep(DASH, math.max(width - left_w - right_w, 0)),
    style = bar_style,
    selectable = false,
  }
  if right_w > 0 then
    segs[#segs + 1] = { text = " ", style = {}, selectable = false }
    join_side(entries, true, segs, plain, {}, bar_style)
    segs[#segs + 1] = { text = " ", style = {}, selectable = false }
    segs[#segs + 1] = { text = DASH, style = bar_style, selectable = false }
  end

  return M.segments_to_line(segs)
end

-- ── bars ────────────────────────────────────────────────────────────

--- Create a bar. `opts.name` labels diagnostics, `opts.layout` is `"strip"` or
--- `"rule"`, and `opts.win` is the window `invalidate` repaints; assign `win`
--- on the returned bar when the window is allocated afterwards. A strip takes
--- `bg_group` and `sep_group`; a rule takes `bar_style`, a function returning
--- the style of the rule (or nil for the default), called on every repaint.
function M.new(opts)
  return {
    name = opts.name,
    layout = opts.layout,
    win = opts.win,
    bg_group = opts.bg_group,
    sep_group = opts.sep_group,
    bar_style = opts.bar_style,
    sources = {}, -- ordered { name, handler } pairs
  }
end

function M.invalidate(bar)
  if bar.win then bar.win:invalidate_renderer() end
end

function M.add(bar, name, handler)
  if type(name) ~= "string" or name == "" then
    error(bar.name .. ".add: name must be a non-empty string", 3)
  end
  if type(handler) ~= "function" then
    error(bar.name .. ".add: handler must be a function", 3)
  end
  local entry = { name = name, handler = handler }
  local replaced = false
  for i, src in ipairs(bar.sources) do
    if src.name == name then
      bar.sources[i] = entry
      replaced = true
      break
    end
  end
  if not replaced then bar.sources[#bar.sources + 1] = entry end
  M.invalidate(bar)
  return smelt.reg.new(function()
    for i, src in ipairs(bar.sources) do
      if src == entry then
        table.remove(bar.sources, i)
        M.invalidate(bar)
        return
      end
    end
  end)
end

function M.remove(bar, name)
  for i = #bar.sources, 1, -1 do
    if bar.sources[i].name == name then table.remove(bar.sources, i) end
  end
  M.invalidate(bar)
end

-- Every source's items in registration order. A source may return a single
-- item instead of a list; a failing source is reported and skipped.
local function collect(bar)
  local items = {}
  for _, src in ipairs(bar.sources) do
    local ok, result = pcall(src.handler)
    if not ok then
      io.stderr:write(bar.name .. " source `" .. src.name .. "`: " .. tostring(result) .. "\n")
    elseif type(result) == "table" then
      if result.text or result.runs then result = { result } end
      for _, item in ipairs(result) do
        items[#items + 1] = item
      end
    end
  end
  return items
end

--- The bar laid out for `width` cells as one row.
function M.row(bar, width)
  local items = collect(bar)
  if bar.layout == "rule" then
    return M.layout_rule(items, width, { bar_style = bar.bar_style and bar.bar_style() })
  end
  return M.layout_strip(items, width, { bg_group = bar.bg_group, sep_group = bar.sep_group })
end

return M
