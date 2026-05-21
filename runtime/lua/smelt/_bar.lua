-- Bar composition primitives: priority-drop fill (prompt top bar +
-- bottom bar), statusline priority-drop layout, traveling-wave color
-- (working indicator), and small text formatters.
--
-- Internal — not part of the public API. `prompt_bar.lua` and
-- `statusline.lua` call into this; plugins that want a custom bar
-- can replace these functions in their own renderer or copy them out.
--
-- A `Span` is the unit input these functions accept:
--   { text, style?, priority?, separated?, truncatable?, align_right? }
-- where `style` is a subset of `smelt.buf.MarkOpts` (`fg`, `bg`,
-- `hl_group`, `bold`, `dim`, `italic`). `fg` / `bg` accept either a
-- theme group name (string) or a direct `{ r, g, b }` triple. The same
-- style table is forwarded verbatim to `buf:mark` so callers don't
-- need to translate.
--
-- Output is `{ text, highlights }` where each highlight carries
-- `bytes_start` / `bytes_end` (byte offsets, the unit `buf:mark`
-- expects) and the forwarded `style`. `write_rows(buf, rows, ns)`
-- projects a list of these rows into a buffer.

local M = {}

local DASH = "\u{2500}"
local STATUS_SEP = " \u{00b7} "
local STATUS_SEP_LEN = 3

-- ── format helpers ──────────────────────────────────────────────────

function M.format_duration(secs)
  if secs < 60 then
    return string.format("%ds", secs)
  elseif secs < 3600 then
    return string.format("%dm %ds", math.floor(secs / 60), secs % 60)
  else
    local hours = math.floor(secs / 3600)
    local minutes = math.floor((secs % 3600) / 60)
    return string.format("%dh %dm %ds", hours, minutes, secs % 60)
  end
end

function M.format_tokens(n)
  if n >= 1000000 then
    return string.format("%.1fm", n / 1000000)
  elseif n >= 1000 then
    return string.format("%.1fk", n / 1000)
  else
    return tostring(n)
  end
end

function M.format_cost(usd)
  if usd < 0.01 then
    return string.format("$%.4f", usd)
  elseif usd < 1.0 then
    return string.format("$%.3f", usd)
  else
    return string.format("$%.2f", usd)
  end
end

-- ── traveling-wave color ────────────────────────────────────────────
--
-- Phase moves left→right as `elapsed_ms` grows. Returns a grayscale
-- RGB triple `{ r, g, b }` between LOW and HIGH. Matches the Rust
-- port `wave_color_at` (PERIOD_MS=1200, WAVELENGTH=16, LOW=140,
-- HIGH=255).

function M.wave_color_at(elapsed_ms, x)
  local PERIOD_MS = 1200.0
  local WAVELENGTH = 16.0
  local HIGH = 255
  local LOW = 140
  local t = elapsed_ms / PERIOD_MS
  local phase = (t - x / WAVELENGTH) * 2 * math.pi
  local intensity = (math.sin(phase) + 1.0) * 0.5
  local level = math.floor(LOW + (HIGH - LOW) * intensity + 0.5)
  return { level, level, level }
end

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
      }
      byte_pos = byte_pos + len
    end
  end
  return { text = table.concat(parts), highlights = highlights }
end

-- ── priority-drop bar row (prompt top + bottom) ─────────────────────

function M.compose(width, left, right)
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
  local left_w = #left_kept > 0 and (sum_w(left_kept) + 1) or 0
  local right_w = #right_kept > 0 and (sum_w(right_kept) + 2) or 0
  local bar_len = math.max(width - left_w - right_w, 0)

  local segs = {}
  for _, s in ipairs(left_kept) do
    segs[#segs + 1] = { text = s.text, style = s.style or {} }
  end
  if #left_kept > 0 then
    segs[#segs + 1] = { text = " ", style = {} }
  end
  segs[#segs + 1] = { text = string.rep(DASH, bar_len), style = { fg = "SmeltBar" } }
  if #right_kept > 0 then
    for _, s in ipairs(right_kept) do
      segs[#segs + 1] = { text = s.text, style = s.style or {} }
    end
    segs[#segs + 1] = { text = " ", style = {} }
    segs[#segs + 1] = { text = DASH, style = { fg = "SmeltBar" } }
  end

  return M.segments_to_line(segs)
end

-- ── statusline priority-drop layout (left + right strips) ───────────
--
-- Mirrors `status::spans_to_buffer_line`. Items split into left/right
-- strips, separated by `" · "` when `separated = true`, truncated
-- with `…` (priority-respecting), then dropped until the line fits.
-- Returns `{ text, highlights }`. `opts` accepts `width` (required),
-- `bg_group` (theme group used to fill empty space + as default span
-- bg), and `sep_group` (theme group used for separator dots).

function M.compose_status(items, opts)
  opts = opts or {}
  local width = opts.width or 80
  local bg_group = opts.bg_group
  local sep_group = opts.sep_group

  -- Defensive deep-clone — truncation mutates `text` and the caller's
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

  local function span_cols(spans, right)
    local w, first = 0, true
    for _, s in ipairs(spans) do
      if (s.align_right or false) == right then
        if s.separated and not first then w = w + STATUS_SEP_LEN end
        w = w + smelt.text.width(s.text)
        first = false
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
        working[trunc_idx].text = smelt.text.fit(working[trunc_idx].text, avail, { suffix = "…" })
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
  local function with_default_bg(style)
    local s = {}
    for k, v in pairs(style) do s[k] = v end
    if bg_group and not s.bg then s.bg = bg_group end
    return s
  end
  local fill_style = with_default_bg({})
  local sep_style = with_default_bg({ fg = sep_group, dim = true })

  local left_runs, right_runs = {}, {}
  local first_left, first_right = true, true
  for _, s in ipairs(working) do
    local runs = s.align_right and right_runs or left_runs
    local first = s.align_right and first_right or first_left
    if s.separated and not first then
      runs[#runs + 1] = { text = STATUS_SEP, style = sep_style }
    end
    runs[#runs + 1] = { text = s.text, style = with_default_bg(s.style) }
    if s.align_right then first_right = false else first_left = false end
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
    segs[#segs + 1] = { text = string.rep(" ", right_start - col), style = fill_style }
    col = right_start
  end
  for _, r in ipairs(right_runs) do
    local w = smelt.text.width(r.text)
    if col + w > width then break end
    segs[#segs + 1] = r
    col = col + w
  end
  if col < width then
    segs[#segs + 1] = { text = string.rep(" ", width - col), style = fill_style }
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
        buf:mark(ns, i, hl.bytes_start, opts)
      end
    end
  end
end

return M
