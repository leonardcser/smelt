-- Bar composition primitives: priority-drop fill (prompt top bar +
-- bottom bar), statusline priority-drop layout, and the
-- traveling-wave color used by the working indicator.
--
-- Internal - not part of the public API. `prompt_bar.lua` and
-- `statusline.lua` call into this; plugins that want a custom bar
-- can replace these functions in their own renderer or copy them out.
-- Human-readable formatters that used to live here (`format_duration`,
-- `format_tokens`, `format_cost`) are now in `smelt.text`.
--
-- A `Span` is the unit input these functions accept:
--   { text, style?, priority?, separated?, truncatable?, truncate?, align_right?, selectable? }
-- where `style` is a subset of `smelt.buf.MarkOpts` (`fg`, `bg`,
-- `hl_group`, `bold`, `dim`, `italic`). `fg` / `bg` accept either a
-- theme group name (string) or a direct `{ r, g, b }` triple. The same
-- style table is forwarded verbatim to `buf:mark` so callers don't
-- need to translate. Set `selectable = false` for visible chrome that
-- should be omitted from drag-copy.
--
-- Output is `{ text, highlights }` where each highlight carries
-- `bytes_start` / `bytes_end` (byte offsets, the unit `buf:mark`
-- expects), the forwarded `style`, and optional `selectable` metadata.
-- `write_rows(buf, rows, ns)` projects a list of these rows into a
-- buffer.

local M = {}

local DASH = "\u{2500}"
local STATUS_SEP = " \u{00b7} "
local STATUS_SEP_LEN = 3

-- ── segments → line + byte-offset highlights ────────────────────────

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

-- ── priority-drop bar row (prompt top + bottom) ─────────────────────

function M.compose(width, left, right, opts)
  opts = opts or {}
  width = math.max(width or 0, 0)
  local min_dashes = 4

  local function inner_width(spans, drop_above)
    if not spans then return 0, 0 end
    local w, count = 0, 0
    for _, s in ipairs(spans) do
      if (s.priority or 0) < drop_above then
        w = w + smelt.text.width(s.text)
        count = count + 1
      end
    end
    return w, count
  end

  local max_pri = 0
  for _, spans in ipairs({ left or {}, right or {} }) do
    for _, s in ipairs(spans) do
      if (s.priority or 0) > max_pri then max_pri = s.priority or 0 end
    end
  end
  local drop_above = max_pri + 1
  while true do
    local linner, lcount = inner_width(left, drop_above)
    local rinner, rcount = inner_width(right, drop_above)
    local left_cells = lcount > 0 and (linner + 1) or 0
    local right_cells = rcount > 0 and (rinner + 2) or 0
    if left_cells + min_dashes + right_cells <= width or drop_above == 1 then
      break
    end
    drop_above = drop_above - 1
  end

  local left_kept, right_kept = {}, {}
  for _, s in ipairs(left or {}) do
    if (s.priority or 0) < drop_above then left_kept[#left_kept + 1] = s end
  end
  for _, s in ipairs(right or {}) do
    if (s.priority or 0) < drop_above then right_kept[#right_kept + 1] = s end
  end

  local function sum_w(spans)
    local n = 0
    for _, s in ipairs(spans) do n = n + smelt.text.width(s.text) end
    return n
  end
  local function side_widths()
    local left_w = #left_kept > 0 and (sum_w(left_kept) + 1) or 0
    local right_w = #right_kept > 0 and (sum_w(right_kept) + 2) or 0
    return left_w, right_w
  end

  local left_w, right_w = side_widths()
  if left_w + right_w > width and #left_kept > 0 then
    left_kept = {}
    left_w, right_w = side_widths()
  end
  if left_w + right_w > width and #right_kept > 0 then
    right_kept = {}
    left_w, right_w = side_widths()
  end
  local bar_len = math.max(width - left_w - right_w, 0)

  local segs = {}
  for _, s in ipairs(left_kept) do
    segs[#segs + 1] = { text = s.text, style = s.style or {}, selectable = s.selectable }
  end
  if #left_kept > 0 then
    segs[#segs + 1] = { text = " ", style = {}, selectable = false }
  end
  local bar_style = opts.bar_style or { fg = "SmeltBar" }
  segs[#segs + 1] = { text = string.rep(DASH, bar_len), style = bar_style, selectable = false }
  if #right_kept > 0 then
    for _, s in ipairs(right_kept) do
      segs[#segs + 1] = { text = s.text, style = s.style or {}, selectable = s.selectable }
    end
    segs[#segs + 1] = { text = " ", style = {}, selectable = false }
    segs[#segs + 1] = { text = DASH, style = bar_style, selectable = false }
  end

  return M.segments_to_line(segs)
end

-- ── statusline priority-drop layout (left + right strips) ───────────
--
-- Mirrors `status::spans_to_buffer_line`. Items split into left/right
-- strips. Items with `separated = true` form an inline group: dots are
-- inserted between group members, never before the first member after
-- an unseparated pill. Text is truncated with `…` (priority-respecting),
-- then dropped until the line fits. Returns `{ text, highlights }`. `opts`
-- accepts `width` (required), `bg_group` (theme group used to fill empty
-- space + as default span bg), and `sep_group` (theme group used for
-- separator dots).

function M.compose_status(items, opts)
  opts = opts or {}
  local width = opts.width or 80
  local bg_group = opts.bg_group
  local sep_group = opts.sep_group

  -- Defensive deep-clone - truncation mutates `text` and the caller's
  -- items must stay intact across frames.
  local working = {}
  for i, it in ipairs(items) do
    local copy = {}
    for k, v in pairs(it) do copy[k] = v end
    -- shallow-clone style too so we can fill in default bg.
    copy.style = {}
    if it.style then for k, v in pairs(it.style) do copy.style[k] = v end end
    working[i] = copy
  end

  local function has_block_bg(s)
    local style = s.style or {}
    return style.bg ~= nil or style.hl_group ~= nil
  end

  local function item_gap(left, right)
    if has_block_bg(left) and has_block_bg(right) then return 0 end
    return 1
  end

  local function fit_middle(text, cells)
    text = text or ""
    cells = math.max(cells or 0, 0)
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

  local function fit_span_text(s, cells)
    if s.truncate == "middle" then return fit_middle(s.text, cells) end
    return smelt.text.fit(s.text, cells, { suffix = "…" })
  end

  local function span_cols(spans, right)
    local w, separated_seen, prev = 0, false, nil
    for _, s in ipairs(spans) do
      if (s.align_right or false) == right then
        if prev then
          if s.separated and separated_seen then
            w = w + STATUS_SEP_LEN
          else
            w = w + item_gap(prev, s)
          end
        end
        if s.separated then separated_seen = true end
        w = w + smelt.text.width(s.text)
        prev = s
      end
    end
    return w
  end

  local function total_width(spans)
    local l = span_cols(spans, false)
    local r = span_cols(spans, true)
    return l + r + (r > 0 and 1 or 0)
  end

  while total_width(working) > width and #working > 0 do
    local max_pri = 0
    for _, s in ipairs(working) do
      if (s.priority or 0) > max_pri then max_pri = s.priority or 0 end
    end
    if max_pri == 0 then break end
    local trunc_idx
    for i = #working, 1, -1 do
      if (working[i].priority or 0) == max_pri and working[i].truncatable then
        trunc_idx = i
        break
      end
    end
    local dropped = false
    if trunc_idx then
      local total = total_width(working)
      local avail = width - (total - smelt.text.width(working[trunc_idx].text))
      if avail >= 2 then
        working[trunc_idx].text = fit_span_text(working[trunc_idx], avail)
      else
        dropped = true
      end
    else
      dropped = true
    end
    if dropped then
      local out = {}
      for _, s in ipairs(working) do
        if (s.priority or 0) ~= max_pri then out[#out + 1] = s end
      end
      working = out
    end
  end

  -- Fill in default bg from `bg_group` for spans that don't set one.
  -- `hl_group` carries its own bg, so a span that picked an `hl_group`
  -- (e.g. a vim/mode pill) keeps that group's bg instead of getting
  -- the statusline fill colour pasted on top.
  local function with_default_bg(style)
    local s = {}
    for k, v in pairs(style) do s[k] = v end
    if bg_group and not s.bg and not s.hl_group then s.bg = bg_group end
    return s
  end
  local fill_style = with_default_bg({})
  local sep_style = with_default_bg({ fg = sep_group, dim = true })

  local left_runs, right_runs = {}, {}
  local left_separated_seen, right_separated_seen = false, false
  local left_prev, right_prev = nil, nil
  for _, s in ipairs(working) do
    local runs = s.align_right and right_runs or left_runs
    local separated_seen = s.align_right and right_separated_seen or left_separated_seen
    local prev = s.align_right and right_prev or left_prev
    if prev then
      if s.separated and separated_seen then
        runs[#runs + 1] = { text = STATUS_SEP, style = sep_style, selectable = false }
      else
        local gap = item_gap(prev, s)
        if gap > 0 then
          runs[#runs + 1] = { text = string.rep(" ", gap), style = fill_style, selectable = false }
        end
      end
    end
    if s.separated then
      if s.align_right then right_separated_seen = true else left_separated_seen = true end
    end
    runs[#runs + 1] = { text = s.text, style = with_default_bg(s.style), selectable = s.selectable }
    if s.align_right then right_prev = s else left_prev = s end
  end

  local right_w = 0
  for _, r in ipairs(right_runs) do right_w = right_w + smelt.text.width(r.text) end
  local right_start = math.max(width - right_w, 0)

  local segs = {}
  local col = 0
  for _, r in ipairs(left_runs) do
    local w = smelt.text.width(r.text)
    if col + w > width then break end
    segs[#segs + 1] = r
    col = col + w
  end
  if col < right_start then
    segs[#segs + 1] = { text = string.rep(" ", right_start - col), style = fill_style, selectable = false }
    col = right_start
  end
  for _, r in ipairs(right_runs) do
    local w = smelt.text.width(r.text)
    if col + w > width then break end
    segs[#segs + 1] = r
    col = col + w
  end
  if col < width then
    segs[#segs + 1] = { text = string.rep(" ", width - col), style = fill_style, selectable = false }
  end

  return M.segments_to_line(segs)
end

-- ── buffer write ────────────────────────────────────────────────────

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

return M
