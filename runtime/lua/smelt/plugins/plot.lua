-- Optional plot tool: line, area, scatter, bar, histogram and heatmap charts drawn as text, sized to the transcript width.

local transcript_defaults = require("smelt.transcript.defaults")
local layout = smelt.layout

-- Series colors cycle through these theme groups. Each carries its color as
-- both fg and bg, so one cell can show a series above another.
local PALETTE = { "SmeltPlot1", "SmeltPlot2", "SmeltPlot3", "SmeltPlot4", "SmeltPlot5", "SmeltPlot6" }
local AXIS = "Comment"

local DEFAULT_HEIGHT, MIN_HEIGHT, MAX_HEIGHT = 12, 3, 40
local MIN_PLOT_COLS = 8

-- Blocks filled from the bottom and from the left, in eighths of a cell.
local UP = { "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█" }
local RIGHT = { "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█" }
-- Braille dot bits by [column][row] inside one 2 x 4 dot cell.
local DOTS = { { 0x01, 0x02, 0x04, 0x40 }, { 0x08, 0x10, 0x20, 0x80 } }

local KINDS = { "line", "area", "scatter", "bar", "histogram", "heatmap" }

local floor, max, min, abs = math.floor, math.max, math.min, math.abs

local function color(series) return PALETTE[(series - 1) % #PALETTE + 1] end

local function round(v) return floor(v + 0.5) end

local function clamp(v, lo, hi) return max(lo, min(hi, v)) end

---------------------------------------------------------------------------
-- Numbers and axes
---------------------------------------------------------------------------

local SUFFIXES = { { 1e12, "T" }, { 1e9, "G" }, { 1e6, "M" }, { 1e3, "k" } }

local function trim_zeros(s)
  if not s:find("%.") then return s end
  return (s:gsub("0+$", ""):gsub("%.$", ""))
end

-- A short label for `v`: 1.25k, 3M, 0.004.
local function format_number(v)
  if v == 0 then return "0" end
  local a = abs(v)
  for _, suffix in ipairs(SUFFIXES) do
    if a >= suffix[1] then return trim_zeros(string.format("%.2f", v / suffix[1])) .. suffix[2] end
  end
  if a >= 100 then return string.format("%.0f", v) end
  if a >= 1 then return trim_zeros(string.format("%.2f", v)) end
  return string.format("%.2g", v)
end

-- The 1, 2, 2.5 or 5 times a power of ten nearest above `raw`.
local function nice_step(raw)
  local base = 10 ^ floor(math.log(raw, 10))
  local f = raw / base
  local nice = f <= 1 and 1 or f <= 2 and 2 or f <= 2.5 and 2.5 or f <= 5 and 5 or 10
  return nice * base
end

-- Widen `lo..hi` to whole multiples of a nice tick step for about `ticks` ticks.
local function nice_range(lo, hi, ticks)
  if lo == hi then
    local pad = lo == 0 and 1 or abs(lo) * 0.1
    lo, hi = lo - pad, hi + pad
    if lo < 0 and hi > 0 and lo + pad >= 0 then lo = 0 end
  end
  local raw = (hi - lo) / max(1, ticks)
  if not (raw > 0 and raw < math.huge) then return nil end
  local step = nice_step(raw)
  if not (step > 0 and step < math.huge) then return nil end
  local low, high = floor(lo / step + 1e-9) * step, math.ceil(hi / step - 1e-9) * step
  if not (high > low and high - low < math.huge) then return nil end
  return low, high, step
end

-- The y axis of a chart `rows` tall over `lo..hi`. `include_zero` keeps 0 on
-- the axis so filled marks start from it.
local function y_axis(lo, hi, rows, include_zero)
  if include_zero then lo, hi = min(lo, 0), max(hi, 0) end
  local step
  lo, hi, step = nice_range(lo, hi, max(1, floor(rows / 3)))
  if not lo then return nil end
  local labels, width = {}, 0
  for i = 0, round((hi - lo) / step) do
    local v = lo + i * step
    local row = clamp(floor((hi - v) / (hi - lo) * rows) + 1, 1, rows)
    if not labels[row] then
      labels[row] = format_number(math.abs(v) < step * 1e-9 and 0 or v)
      width = max(width, smelt.text.width(labels[row]))
    end
  end
  return { lo = lo, hi = hi, labels = labels, width = width }
end

---------------------------------------------------------------------------
-- Cells and rows
---------------------------------------------------------------------------

-- A row is a list of cells `{ ch, fg?, bg?, hl? }`; runs of equal style merge
-- into one span.
local function row_line(cells)
  local spans, current = {}, nil
  for _, cell in ipairs(cells) do
    if current and current.fg == cell.fg and current.bg == cell.bg and current.hl == cell.hl then
      current.text = current.text .. cell.ch
    else
      current = { text = cell.ch, fg = cell.fg, bg = cell.bg, hl = cell.hl or "Normal" }
      spans[#spans + 1] = current
    end
  end
  return layout.line(spans)
end

local function text_cells(cells, text, style)
  for _, cp in utf8.codes(text) do
    local cell = { ch = utf8.char(cp) }
    for k, v in pairs(style or {}) do cell[k] = v end
    cells[#cells + 1] = cell
  end
  return cells
end

local function blank(n)
  local cells = {}
  for i = 1, n do cells[i] = { ch = " " } end
  return cells
end

-- One strip of cells filled by stacked bands. `bands` lists `{ top, series }`
-- in eighths, ascending; each band runs from the previous top to its own.
-- Where a band ends inside a cell, the cell shows the band below as a partial
-- block over the band above, so no band boundary snaps to whole cells.
local function strip(bands, count, glyphs)
  -- The band covering eighth `e`, if any.
  local function band_at(e)
    for _, band in ipairs(bands) do
      if band.top > e then return band end
    end
  end
  local cells = {}
  for i = 1, count do
    local base = (i - 1) * 8
    local lower = band_at(base)
    if not lower then
      cells[i] = { ch = " " }
    else
      local filled = min(8, lower.top - base)
      local upper = filled < 8 and band_at(base + filled)
      cells[i] = {
        ch = glyphs[filled],
        fg = color(lower.series),
        bg = upper and color(upper.series) or nil,
      }
    end
  end
  return cells
end

-- Place `marks` (`{ col, text }`, col 1-based) on a row `width` cells wide:
-- the first left-aligned, the last right-aligned, the rest centered. Marks
-- that would touch a placed one are dropped.
local function tick_row(marks, width)
  local row = {}
  for i = 1, width do row[i] = " " end
  local taken = {}
  local function free(from, to)
    if from < 1 or to > width then return false end
    for i = from - 1, to + 1 do
      if taken[i] then return false end
    end
    return true
  end
  local function put(from, text)
    row[from] = text
    for col = from, from + smelt.text.width(text) - 1 do
      taken[col] = true
      if col > from then row[col] = "" end
    end
  end
  for i, mark in ipairs(marks) do
    local w = smelt.text.width(mark.text)
    local from
    if i == 1 then
      from = clamp(mark.col, 1, max(1, width - w + 1))
    elseif i == #marks then
      from = clamp(mark.col - w + 1, 1, max(1, width - w + 1))
    else
      from = mark.col - floor((w - 1) / 2)
    end
    if free(from, from + w - 1) then put(from, mark.text) end
  end
  return table.concat(row)
end

---------------------------------------------------------------------------
-- Arguments
---------------------------------------------------------------------------

local function is_number(v)
  return type(v) == "number" and v == v and v ~= math.huge and v ~= -math.huge
end

local function number_list(list, name, allow_empty)
  if type(list) ~= "table" then return nil, name .. " must be an array of numbers" end
  if #list == 0 and not allow_empty then return nil, name .. " must not be empty" end
  local numbers = {}
  for i, v in ipairs(list) do
    if not is_number(v) then return nil, string.format("%s[%d] must be a finite number", name, i) end
    numbers[i] = v + 0.0 -- Lua integer arithmetic wraps instead of overflowing to infinity.
  end
  return numbers
end

local function string_list(list, name)
  if type(list) ~= "table" or #list == 0 then return nil, name .. " must be a non-empty array of strings" end
  local out = {}
  for i, v in ipairs(list) do
    if type(v) ~= "string" and type(v) ~= "number" then
      return nil, string.format("%s[%d] must be a string", name, i)
    end
    out[i] = tostring(v)
  end
  return out
end

local function optional_string(v)
  return type(v) == "string" and v ~= "" and v or nil
end

local function parse_series(args, count)
  if type(args.series) ~= "table" or #args.series == 0 then
    return nil, "series must be a non-empty array of { name?, y, x? }"
  end
  local series = {}
  for i, raw in ipairs(args.series) do
    local name = string.format("series[%d]", i)
    if type(raw) ~= "table" then return nil, name .. " must be an object" end
    local y, err = number_list(raw.y, name .. ".y")
    if not y then return nil, err end
    if count and #y ~= count then
      return nil, string.format("%s.y has %d values; expected %d", name, #y, count)
    end
    local x
    if raw.x ~= nil and not (type(raw.x) == "table" and next(raw.x) == nil) then
      x, err = number_list(raw.x, name .. ".x")
      if not x then return nil, err end
      if #x ~= #y then return nil, string.format("%s.x and %s.y differ in length", name, name) end
    end
    series[i] = { name = optional_string(raw.name), x = x, y = y }
  end
  return series
end

-- Check the tool arguments. Returns a normalized spec, or nil and the reason.
local function parse_args(args)
  local kind = args.kind
  local known = false
  for _, k in ipairs(KINDS) do known = known or k == kind end
  if not known then return nil, "kind must be one of: " .. table.concat(KINDS, ", ") end
  local spec = {
    kind = kind,
    x_label = optional_string(args.x_label),
    y_label = optional_string(args.y_label),
    height = clamp(floor(tonumber(args.height) or DEFAULT_HEIGHT), MIN_HEIGHT, MAX_HEIGHT),
  }
  local err
  spec.percent = args.percent == true
  if spec.percent and ((kind ~= "area" and kind ~= "bar") or args.stacked == false) then
    return nil, "percent requires a stacked area or bar chart"
  end
  if kind == "heatmap" then
    spec.labels, err = string_list(args.labels, "labels")
    if not spec.labels then return nil, err end
    spec.x_labels, err = string_list(args.x_labels, "x_labels")
    if not spec.x_labels then return nil, err end
    spec.matrix = args.matrix
    if type(spec.matrix) ~= "table" or #spec.matrix ~= #spec.labels then return nil, "matrix needs one row per label" end
    spec.peak, spec.signed = 0, false
    for _, row in ipairs(spec.matrix) do
      if type(row) ~= "table" or #row ~= #spec.x_labels then return nil, "matrix rows must match x_labels" end
      for _, v in ipairs(row) do
        if v ~= "NA" then
          if not is_number(v) then return nil, "heatmap cells must be finite numbers or NA" end
          spec.peak = max(spec.peak, abs(v + 0.0))
          spec.signed = spec.signed or v < 0
        end
      end
    end
  elseif kind == "histogram" then
    spec.values, err = number_list(args.values, "values")
    if not spec.values then return nil, err end
    if args.bins ~= nil and not (is_number(args.bins) and args.bins % 1 == 0 and args.bins >= 1 and args.bins <= 50) then
      return nil, "bins must be an integer from 1 to 50"
    end
    spec.bins = args.bins
  elseif kind == "bar" then
    spec.labels, err = string_list(args.labels, "labels")
    if not spec.labels then return nil, err end
    if args.series ~= nil and not (type(args.series) == "table" and next(args.series) == nil) then
      spec.series, err = parse_series(args, #spec.labels)
    else
      local values
      values, err = number_list(args.values, "values")
      if values and #values ~= #spec.labels then
        values, err = nil, string.format("values has %d entries; labels has %d", #values, #spec.labels)
      end
      spec.series = values and { { y = values } }
    end
    if not spec.series then return nil, err end
    spec.stacked = args.stacked ~= false
    if spec.stacked and #spec.series > 1 then
      for i, s in ipairs(spec.series) do
        for j, v in ipairs(s.y) do
          if v < 0 then return nil, string.format("series[%d].y[%d] is negative; stacked bars need values >= 0", i, j) end
        end
      end
    end
  else
    spec.series, err = parse_series(args)
    if not spec.series then return nil, err end
    if args.x_labels ~= nil then
      spec.x_labels, err = string_list(args.x_labels, "x_labels")
      if not spec.x_labels then return nil, err end
      if #spec.x_labels ~= #spec.series[1].y then return nil, "x_labels must match the first series length" end
    end
    if kind == "area" then
      spec.stacked = args.stacked ~= false
    else
      spec.stacked = kind == "line" and args.stacked == true
    end
    if spec.stacked and #spec.series > 1 then
      local n = #spec.series[1].y
      for i, s in ipairs(spec.series) do
        if #s.y ~= n then
          return nil, string.format("stacked series need equal lengths: series[%d] has %d values, series[1] has %d", i, #s.y, n)
        end
        for j, v in ipairs(s.y) do
          local shared_x = spec.series[1].x
          if s.x and s.x[j] ~= (shared_x and shared_x[j] or j) then return nil, "stacked series must share x-values" end
          if v < 0 then return nil, string.format("series[%d].y[%d] is negative; stacked series need values >= 0", i, j) end
        end
      end
    end
  end
  return spec
end

---------------------------------------------------------------------------
-- Charts
---------------------------------------------------------------------------

-- Points of each series as sorted `{ x, y }`; stacked series sum by index.
local function series_points(spec)
  local out = {}
  local running = {}
  for s, series in ipairs(spec.series) do
    local pts = {}
    for i, y in ipairs(series.y) do
      local x = series.x and series.x[i] or (spec.stacked and spec.series[1].x and spec.series[1].x[i]) or i
      if spec.stacked then
        running[i] = (running[i] or 0) + y
        y = running[i]
      end
      pts[i] = { x = x, y = y, gap = spec.gaps and spec.gaps[i] }
    end
    table.sort(pts, function(a, b) return a.x < b.x end)
    out[s] = pts
  end
  return out
end

local function bounds(points)
  local xmin, xmax, ymin, ymax = math.huge, -math.huge, math.huge, -math.huge
  for _, pts in ipairs(points) do
    for _, p in ipairs(pts) do
      xmin, xmax = min(xmin, p.x), max(xmax, p.x)
      ymin, ymax = min(ymin, p.y), max(ymax, p.y)
    end
  end
  return xmin, xmax, ymin, ymax
end

-- Prepare width-independent data in the same path for execution and rendering.
local function parse(args)
  local spec, err = parse_args(args)
  if not spec then return nil, err end
  if spec.kind == "heatmap" then return spec end
  local range_error = "numeric range is too large or too small to plot; rescale the data"
  if spec.percent then
    spec.gaps = {}
    for i = 1, #spec.series[1].y do
      local total = 0
      for _, s in ipairs(spec.series) do
        if s.y[i] < 0 then return nil, "percent needs nonnegative values" end
        total = total + s.y[i]
      end
      if not is_number(total) then return nil, range_error end
      spec.gaps[i] = total == 0
      for _, s in ipairs(spec.series) do s.y[i] = total == 0 and 0 or s.y[i] / total * 100 end
    end
  end
  if spec.kind == "histogram" then
    local lo, hi = math.huge, -math.huge
    for _, v in ipairs(spec.values) do lo, hi = min(lo, v), max(hi, v) end
    if not is_number(hi - lo) then return nil, range_error end
    spec.xmin, spec.xmax = lo, hi
  elseif spec.kind == "bar" then
    local lo, hi = 0, 0
    for i = 1, #spec.labels do
      local total = 0
      for _, s in ipairs(spec.series) do
        total = (spec.stacked and total or 0) + s.y[i]
        lo, hi = min(lo, total), max(hi, total)
      end
    end
    if not is_number(hi - lo) then return nil, range_error end
  else
    spec.points = series_points(spec)
    local xmin, xmax, ymin, ymax = bounds(spec.points)
    if spec.kind == "area" and ymin < 0 then return nil, "area needs nonnegative values; use line for signed data" end
    if not is_number(xmax - xmin) then return nil, range_error end
    if spec.percent then
      spec.axis = { lo = 0, hi = 100, width = 4, labels = { [1] = "100%", [spec.height] = "0%" } }
    else
      spec.axis = y_axis(ymin, ymax, spec.height, spec.kind == "area")
    end
    if not spec.axis then return nil, range_error end
    spec.xmin, spec.xmax = xmin, xmax
  end
  return spec
end

-- Linear interpolation of sorted `pts` at `x`; nil outside their x range.
local function value_at(pts, x)
  if #pts == 0 or x < pts[1].x or x > pts[#pts].x then return nil end
  for i = 2, #pts do
    local a, b = pts[i - 1], pts[i]
    if x <= b.x then
      if a.gap or b.gap then
        local nearest = x - a.x <= b.x - x and a or b
        if nearest.gap then return nil end
        return nearest.y
      end
      if b.x == a.x then return b.y end
      return a.y + (b.y - a.y) * ((x - a.x) / (b.x - a.x))
    end
  end
  return pts[#pts].y
end

-- Braille line and scatter marks: 2 x 4 dots per cell, later series on top.
local function braille_cells(points, cols, rows, xmin, xmax, axis, connect)
  local grid = {}
  local dot_cols, dot_rows = cols * 2, rows * 4
  local function to_px(x)
    if xmax == xmin then return floor(dot_cols / 2) end
    return round((x - xmin) / (xmax - xmin) * (dot_cols - 1))
  end
  local function to_py(y)
    return round((axis.hi - y) / (axis.hi - axis.lo) * (dot_rows - 1))
  end
  local function dot(px, py, series)
    if px < 0 or px >= dot_cols or py < 0 or py >= dot_rows then return end
    local i = (py // 4) * cols + px // 2 + 1
    local cell = grid[i] or { bits = 0 }
    cell.bits = cell.bits | DOTS[px % 2 + 1][py % 4 + 1]
    cell.series = series
    grid[i] = cell
  end
  for s, pts in ipairs(points) do
    local px0, py0
    for _, p in ipairs(pts) do
      local px, py = to_px(p.x), to_py(p.y)
      if connect and px0 then
        -- Bresenham from the previous point.
        local dx, dy = abs(px - px0), -abs(py - py0)
        local sx, sy = px0 < px and 1 or -1, py0 < py and 1 or -1
        local err, x, y = dx + dy, px0, py0
        while true do
          dot(x, y, s)
          if x == px and y == py then break end
          local e2 = 2 * err
          if e2 >= dy then err, x = err + dy, x + sx end
          if e2 <= dx then err, y = err + dx, y + sy end
        end
      else
        dot(px, py, s)
      end
      px0, py0 = px, py
    end
  end
  local rows_out = {}
  for r = 1, rows do
    local cells = {}
    for c = 1, cols do
      local cell = grid[(r - 1) * cols + c]
      cells[c] = cell and { ch = utf8.char(0x2800 + cell.bits), fg = color(cell.series) } or { ch = " " }
    end
    rows_out[r] = cells
  end
  return rows_out
end

-- Vertical filled columns: `column_bands(c)` returns the bands of column `c`
-- in value units, and they are drawn in eighths of a row.
local function column_cells(cols, rows, axis, column_bands)
  local span = axis.hi - axis.lo
  local columns = {}
  for c = 1, cols do
    local bands = {}
    for _, band in ipairs(column_bands(c) or {}) do
      bands[#bands + 1] = { top = round((band.top - axis.lo) / span * rows * 8), series = band.series }
    end
    columns[c] = strip(bands, rows, UP)
  end
  local out = {}
  for r = 1, rows do
    local cells = {}
    for c = 1, cols do cells[c] = columns[c][rows - r + 1] end
    out[r] = cells
  end
  return out
end

-- The bands of one area column: stacked series pile up in order; overlapping
-- series draw the smaller in front.
local function area_bands(points, x, stacked)
  local values = {}
  for s, pts in ipairs(points) do
    local v = value_at(pts, x)
    if v then values[#values + 1] = { top = v, series = s } end
  end
  if not stacked then table.sort(values, function(a, b) return a.top < b.top end) end
  return values
end

-- Frame plot `cells` rows with the y axis, x axis and x tick labels.
local function framed(spec, cells, axis, x_marks, cols)
  local lines = {}
  for r, row in ipairs(cells) do
    local label = axis.labels[r]
    local left = text_cells({}, smelt.text.fit(label or "", axis.width, { align = "right" }), { hl = AXIS })
    text_cells(left, label and "┤" or "│", { hl = AXIS })
    for _, cell in ipairs(row) do left[#left + 1] = cell end
    lines[#lines + 1] = row_line(left)
  end
  lines[#lines + 1] = row_line(text_cells({}, string.rep(" ", axis.width) .. "└" .. string.rep("─", cols), { hl = AXIS }))
  if x_marks and #x_marks > 0 then
    lines[#lines + 1] = row_line(text_cells({}, string.rep(" ", axis.width + 1) .. tick_row(x_marks, cols), { hl = AXIS }))
  end
  if spec.x_label then
    local pad = axis.width + 1 + max(0, floor((cols - smelt.text.width(spec.x_label)) / 2))
    lines[#lines + 1] = row_line(text_cells({}, string.rep(" ", pad) .. spec.x_label, { hl = AXIS }))
  end
  return lines
end

-- x tick marks for numeric x, or for `x_labels` by point index.
local function x_marks(spec, cols, xmin, xmax)
  local marks = {}
  local function col_of(x)
    if xmax == xmin then return floor(cols / 2) + 1 end
    return round((x - xmin) / (xmax - xmin) * (cols - 1)) + 1
  end
  if spec.x_labels then
    local n = #spec.x_labels
    local picks = n == 1 and { 1 } or n == 2 and { 1, 2 } or { 1, (n + 1) // 2, n }
    for _, i in ipairs(picks) do
      local x = spec.series[1].x and spec.series[1].x[i] or i
      marks[#marks + 1] = { col = col_of(x), text = spec.x_labels[i] }
    end
  else
    local picks = xmin == xmax and { xmin } or { xmin, xmin + (xmax - xmin) / 2, xmax }
    for _, x in ipairs(picks) do marks[#marks + 1] = { col = col_of(x), text = format_number(x) } end
  end
  return marks
end

local function xy_chart(spec, width)
  local points, axis = spec.points, spec.axis
  local xmin, xmax = spec.xmin, spec.xmax
  local filled = spec.kind == "area"
  local cols = width - axis.width - 1
  if cols < MIN_PLOT_COLS then return nil end
  local cells
  if filled then
    cells = column_cells(cols, spec.height, axis, function(c)
      local x = xmax == xmin and xmin or xmin + (c - 0.5) / cols * (xmax - xmin)
      return area_bands(points, x, spec.stacked)
    end)
  else
    cells = braille_cells(points, cols, spec.height, xmin, xmax, axis, spec.kind == "line")
  end
  return framed(spec, cells, axis, x_marks(spec, cols, xmin, xmax), cols)
end

local function histogram_chart(spec, width)
  local values = spec.values
  local vmin, vmax = spec.xmin, spec.xmax
  local bins = spec.bins or clamp(math.ceil(math.log(#values, 2)) + 1, 1, 50)
  local span = vmax - vmin
  local counts, axis, cols
  while true do
    counts = {}
    local top = 0
    for i = 1, bins do counts[i] = 0 end
    for _, v in ipairs(values) do
      local i = span == 0 and 1 or min(bins, floor((v - vmin) / span * bins) + 1)
      counts[i] = counts[i] + 1
      top = max(top, counts[i])
    end
    axis = y_axis(0, top, spec.height, true)
    cols = width - axis.width - 1
    if cols < MIN_PLOT_COLS then return nil end
    if bins <= cols then break end
    bins = cols -- Re-bin the entire domain; never clip samples from either end.
  end
  local gap = cols // bins >= 3
  local cells = column_cells(cols, spec.height, axis, function(c)
    local bin = min(bins, (c - 1) * bins // cols + 1)
    local next_bin = c * bins // cols + 1
    if gap and next_bin > bin and c < cols then return nil end
    return { { top = counts[bin], series = 1 } }
  end)
  local marks = {
    { col = 1, text = format_number(vmin) },
    { col = floor(cols / 2) + 1, text = format_number(vmin + span / 2) },
    { col = cols, text = format_number(vmax) },
  }
  return framed(spec, cells, axis, marks, cols)
end

local function bar_chart(spec, width)
  local series, labels = spec.series, spec.labels
  local stacked = spec.stacked and #series > 1
  -- One row per label, or per label and series when grouped.
  local rows = {}
  for i, label in ipairs(labels) do
    if stacked or #series == 1 then
      local bands, total = {}, 0
      for s, ser in ipairs(series) do
        total = total + ser.y[i]
        bands[#bands + 1] = { top = total, series = s }
      end
      rows[#rows + 1] = { label = label, bands = bands, value = total }
    else
      for s, ser in ipairs(series) do
        rows[#rows + 1] = { label = s == 1 and label or "", bands = { { top = ser.y[i], series = s } }, value = ser.y[i] }
      end
    end
  end
  local lo, hi, label_w, value_w = 0, 0, 0, 0
  for _, row in ipairs(rows) do
    lo, hi = min(lo, row.value), max(hi, row.value)
    label_w = max(label_w, smelt.text.width(row.label))
    row.caption = spec.percent and (row.value == 0 and "n/a" or "100%") or format_number(row.value)
    value_w = max(value_w, smelt.text.width(row.caption))
  end
  label_w = min(label_w, max(4, width // 3))
  local bar_w = width - label_w - value_w - 3
  if bar_w < MIN_PLOT_COLS then return nil end
  local range = hi - lo
  if range == 0 then range = 1 end
  local neg_w = lo < 0 and max(1, round((-lo / range) * bar_w)) or 0
  local pos_w = bar_w - neg_w
  local lines = {}
  for _, row in ipairs(rows) do
    local cells = text_cells({}, smelt.text.fit(row.label, label_w), { hl = "Normal" })
    text_cells(cells, " ", nil)
    local value = row.value
    if neg_w > 0 then
      local filled = value < 0 and round(value / lo * neg_w) or 0
      local pad = blank(neg_w - filled)
      for _, c in ipairs(pad) do cells[#cells + 1] = c end
      for _ = 1, filled do cells[#cells + 1] = { ch = "█", fg = color(row.bands[1].series) } end
    end
    text_cells(cells, "│", { hl = AXIS })
    if value > 0 then
      local bands = {}
      for _, band in ipairs(row.bands) do
        bands[#bands + 1] = { top = round(band.top / hi * pos_w * 8), series = band.series }
      end
      for _, c in ipairs(strip(bands, math.ceil(bands[#bands].top / 8), RIGHT)) do cells[#cells + 1] = c end
    end
    text_cells(cells, " " .. row.caption, { hl = AXIS })
    lines[#lines + 1] = row_line(cells)
  end
  return lines
end

-- Heatmap ramp groups. Each carries its color as both fg and bg.
local ZERO = "SmeltPlotZero"
local POS = { "SmeltPlotPos1", "SmeltPlotPos2", "SmeltPlotPos3", "SmeltPlotPos4", "SmeltPlotPos5" }
local NEG = { "SmeltPlotNeg1", "SmeltPlotNeg2", "SmeltPlotNeg3", "SmeltPlotNeg4", "SmeltPlotNeg5" }
local HALF_BLOCK = 2 -- Matrix rows one text row holds in pixel mode.
local MAX_PIXEL = 4 -- Widest pixel-mode cell, in columns.

-- The ramp group for `v`, and whether it is dark enough to need light text.
-- Steps grow with |v| / peak; negative values take the second hue.
local function heat(v, peak)
  if v == 0 or peak == 0 then return ZERO, false end
  local ramp = v < 0 and NEG or POS
  local step = clamp(math.ceil(abs(v) / peak * #ramp), 1, #ramp)
  return ramp[step], step > #ramp // 2
end

-- A tile value without the leading zero: .62, -.7, 1.2k.
local function tile_label(v)
  return (format_number(v):gsub("^(-?)0%.", "%1."))
end

local function append(cells, more)
  for _, c in ipairs(more) do cells[#cells + 1] = c end
end

-- Column labels over a plot area `plot_w` wide, one mark per cell `cell_w` wide.
local function pixel_header(labels, cell_w, label_w, plot_w)
  local marks = {}
  for j, label in ipairs(labels) do
    if label ~= "" then marks[#marks + 1] = { col = (j - 1) * cell_w + (cell_w + 1) // 2, text = label } end
  end
  local row = tick_row(marks, plot_w)
  if not row:find("%S") then return nil end
  return layout.line({ { text = string.rep(" ", label_w + 1) .. row:gsub("%s+$", ""), hl = AXIS } })
end

-- Square tiles with the value in each: one column of padding each side, and
-- half as many rows as columns, since terminal cells are about twice as tall
-- as wide.
local function heat_tiles(spec, label_w, tile_w)
  local tile_h = (tile_w + 1) // 2
  local mid = (tile_h + 1) // 2
  local header = string.rep(" ", label_w + 1)
  for _, label in ipairs(spec.x_labels) do
    header = header .. smelt.text.fit(label, tile_w, { align = "center", suffix = "" })
  end
  local lines = { layout.line({ { text = header:gsub("%s+$", ""), hl = AXIS } }) }
  for r, row in ipairs(spec.matrix) do
    for y = 1, tile_h do
      local label = y == mid and spec.labels[r] or ""
      local cells = text_cells({}, smelt.text.fit(label, label_w, { align = "right" }) .. " ", nil)
      for _, v in ipairs(row) do
        if v == "NA" then
          append(cells, blank(tile_w))
        else
          local group, strong = heat(v, spec.peak)
          local text = y == mid and smelt.text.fit(tile_label(v), tile_w, { align = "center" }) or string.rep(" ", tile_w)
          text_cells(cells, text, { bg = group, fg = strong and ZERO or nil })
        end
      end
      lines[#lines + 1] = row_line(cells)
    end
  end
  return lines
end

-- Average each run of `k` columns, skipping NA, so the matrix fits the width.
local function bin_columns(spec, k)
  local matrix, labels = {}, {}
  for j = 1, #spec.x_labels, k do labels[#labels + 1] = spec.x_labels[j] end
  for r, row in ipairs(spec.matrix) do
    matrix[r] = {}
    for j = 1, #row, k do
      local sum, count = 0, 0
      for i = j, min(#row, j + k - 1) do
        if row[i] ~= "NA" then sum, count = sum + row[i], count + 1 end
      end
      matrix[r][#matrix[r] + 1] = count > 0 and sum / count or "NA"
    end
  end
  return matrix, labels
end

-- Square cells of half-block pixels, `c` columns by `c` half rows. A single
-- pixel packs two matrix rows into one text row; larger cells stay even so
-- each matrix row starts on a text row of its own.
local function heat_pixels(spec, label_w, plot_w)
  local k = math.ceil(#spec.x_labels / plot_w)
  local matrix, x_labels = spec.matrix, spec.x_labels
  if k > 1 then matrix, x_labels = bin_columns(spec, k) end
  local c = plot_w // #x_labels
  c = c >= HALF_BLOCK and min(MAX_PIXEL, c - c % HALF_BLOCK) or 1
  local pixels = {}
  for _, row in ipairs(matrix) do
    local line = {}
    for _, v in ipairs(row) do
      local group = v ~= "NA" and heat(v, spec.peak)
      for _ = 1, c do line[#line + 1] = group end
    end
    for _ = 1, c do pixels[#pixels + 1] = line end
  end
  local lines = { pixel_header(x_labels, c, label_w, plot_w) }
  for y = 1, #pixels, HALF_BLOCK do
    local label = (y - 1) % c == 0 and spec.labels[(y - 1) // c + 1] or ""
    local cells = text_cells({}, smelt.text.fit(label, label_w, { align = "right" }) .. " ", nil)
    local top, bottom = pixels[y], pixels[y + 1] or {}
    for x, t in ipairs(top) do
      local b = bottom[x]
      if t and b then
        cells[#cells + 1] = t == b and { ch = " ", bg = t } or { ch = "▀", fg = t, bg = b }
      elseif t or b then
        cells[#cells + 1] = { ch = t and "▀" or "▄", fg = t or b }
      else
        cells[#cells + 1] = { ch = " " }
      end
    end
    lines[#lines + 1] = row_line(cells)
  end
  return lines, k
end

-- The ramp from the lowest to the highest value it encodes.
local function heat_legend(spec, label_w, binned)
  local cells = text_cells({}, string.rep(" ", label_w + 1), nil)
  text_cells(cells, tile_label(spec.signed and -spec.peak or 0) .. " ", { hl = AXIS })
  local steps = {}
  if spec.signed then
    for i = #NEG, 1, -1 do steps[#steps + 1] = NEG[i] end
  end
  steps[#steps + 1] = ZERO
  for _, group in ipairs(POS) do steps[#steps + 1] = group end
  for _, group in ipairs(steps) do text_cells(cells, "  ", { bg = group }) end
  text_cells(cells, " " .. tile_label(spec.peak), { hl = AXIS })
  if binned > 1 then text_cells(cells, string.format("   %d columns per cell", binned), { hl = AXIS }) end
  return row_line(cells)
end

-- Values in square tiles when they fit the width; otherwise a dense grid of
-- half-block pixels. Signed data takes a two-hue ramp centered on zero.
local function heatmap_chart(spec, width)
  local label_w = 0
  for _, label in ipairs(spec.labels) do label_w = max(label_w, smelt.text.width(label)) end
  label_w = min(label_w, width // 4)
  local plot_w = width - label_w - 1
  if plot_w < MIN_PLOT_COLS then return nil end
  local value_w = 1
  for _, row in ipairs(spec.matrix) do
    for _, v in ipairs(row) do
      if v ~= "NA" then value_w = max(value_w, smelt.text.width(tile_label(v))) end
    end
  end
  local tile_w = value_w + 2
  local lines, binned
  if #spec.x_labels * tile_w <= plot_w then
    lines, binned = heat_tiles(spec, label_w, tile_w), 1
  else
    lines, binned = heat_pixels(spec, label_w, plot_w)
  end
  lines[#lines + 1] = heat_legend(spec, label_w, binned)
  return lines
end

local function legend(spec)
  local named = false
  for _, s in ipairs(spec.series or {}) do named = named or s.name ~= nil end
  if not named then return nil end
  local glyph = (spec.kind == "line" or spec.kind == "scatter") and "⣿ " or "█ "
  local cells = {}
  for i, s in ipairs(spec.series) do
    if i > 1 then text_cells(cells, "   ", nil) end
    text_cells(cells, glyph, { fg = color(i) })
    text_cells(cells, s.name or string.format("series %d", i), { hl = "Normal" })
  end
  return row_line(cells)
end

-- The chart for `spec` at `width` columns, as a vertical stack of rows.
local function render(spec, width)
  local lines = {}
  if spec.y_label then lines[#lines + 1] = layout.line({ { text = spec.y_label, hl = AXIS } }) end
  local chart
  if spec.kind == "heatmap" then
    chart = heatmap_chart(spec, width)
  elseif spec.kind == "bar" then
    chart = bar_chart(spec, width)
  elseif spec.kind == "histogram" then
    chart = histogram_chart(spec, width)
  else
    chart = xy_chart(spec, width)
  end
  if not chart then return layout.line(smelt.text.truncate_cells("Widen pane to see plot", width)) end
  for _, line in ipairs(chart) do lines[#lines + 1] = line end
  local key = legend(spec)
  if key then lines[#lines + 1] = key end
  return layout.vbox(lines)
end

---------------------------------------------------------------------------
-- Tool
---------------------------------------------------------------------------

smelt.transcript.register_tool("plot", {
  body = function(block, ctx, opts)
    local output = block.output
    if not output then return nil end
    local spec = not transcript_defaults.child_failed(block) and parse(block.args or {})
    if not spec then return transcript_defaults.render_tool_output_tail(output, ctx) end
    return render(spec, opts.width)
  end,
})

local series_schema = {
  type = "array",
  description = "Data series. Bars align each series' y with labels.",
  items = {
    type = "object",
    properties = {
      name = { type = "string", description = "Legend name" },
      y = { type = "array", items = { type = "number" } },
      x = { type = "array", items = { type = "number" }, description = "One x per y. Omit or pass [] for automatic x coordinates." },
    },
    required = { "y" },
  },
}

smelt.tools.register({
  name = "plot",
  description = "Show the user a chart of numeric data, drawn as text in the terminal at its full width. "
    .. "Kinds: `line` (trends; `stacked` sums the series), `area` (filled; several series stack so each band "
    .. "is a part and the top edge is the total; `stacked = false` overlaps them), `scatter` (points), "
    .. "`bar` (one horizontal bar per label; several series stack, or group with `stacked = false`), "
    .. "`histogram` (distribution of raw `values`) and `heatmap` (a `matrix` with row `labels` and `x_labels`, "
    .. "such as correlations or activity by weekday and hour; small matrices show each value, large ones a dense grid). The user sees the chart; you get no image back.",
  access = "read",
  headless = false,
  parameters = {
    type = "object",
    properties = {
      kind = { type = "string", enum = KINDS },
      title = { type = "string" },
      series = series_schema,
      labels = { type = "array", items = { type = "string" }, description = "bar: category labels; heatmap: row labels" },
      values = { type = "array", items = { type = "number" }, description = "bar: one value per label; histogram: raw samples" },
      x_labels = { type = "array", items = { type = "string" }, description = "line/area/scatter: label per point index, such as dates; heatmap: column labels" },
      x_label = { type = "string", description = "x axis title" },
      y_label = { type = "string", description = "y axis title" },
      stacked = { type = "boolean" },
      percent = { type = "boolean", description = "Stacked area/bar: show each part as a percentage of the sample total. Zero totals are gaps." },
      matrix = { type = "array", description = "heatmap: one row per labels entry, one cell per x_labels entry. Use NA for missing data.",
        items = { type = "array", items = { anyOf = { { type = "number" }, { type = "string", enum = { "NA" } } } } } },
      bins = { type = "integer", description = "histogram bin count" },
      height = { type = "integer", description = "Chart rows, 3 to 40 (default 12)" },
    },
    required = { "kind" },
  },
  summary = function(args)
    return args.title or args.kind or ""
  end,
  execute = function(args)
    local spec, err = parse(args)
    if not spec then return { content = err, is_error = true } end
    return { content = "shown to the user" }
  end,
})
