-- F12 perf panel. Top-right overlay showing live duration percentiles.
-- Non-modal and non-focusable; F12 again to dismiss.

local M = {}

local PANEL = nil
local NS_HL = smelt.buf.create_namespace("smelt.perf_panel")

local PANEL_W = 44
local PANEL_H = 14
-- STATS_W = right-hand "last  p99   n" cluster width; label column = inner_w - STATS_W.
local STATS_W = 19
local MIN_LABEL_W = 6

-- Maps a duration (µs) to a theme highlight role.
local function severity_role(us)
  if us < 100 then return "Comment" end                  -- < 100µs (dim)
  if us < 1000 then return "SmeltReasonLow" end          -- < 1ms (cyan/blue)
  if us < 5000 then return "SmeltReasonMed" end          -- < 5ms (orange)
  if us < 16000 then return "SmeltReasonHigh" end        -- < 16ms (red)
  return "SmeltReasonMax"                                -- ≥ 16ms (bright red)
end

local function fmt_us(us)
  if us < 1000 then return string.format("%4dµs", us) end
  local ms = us / 1000.0
  if ms < 10 then return string.format("%4.2fms", ms) end
  if ms < 100 then return string.format("%4.1fms", ms) end
  return string.format("%4dms", math.floor(ms + 0.5))
end

local function pad_label(label, label_w)
  local len = #label
  if len > label_w then
    return label:sub(1, label_w - 1) .. "…"
  end
  return label .. string.rep(" ", label_w - len)
end

-- Layout: label | last(6) | p99(6) | n(3). Header right-aligns within the same widths.
local function header_for(label_w)
  local label_col = pad_label("label", label_w)
  local last_col = string.format("%6s", "last")
  local p99_col = string.format("%6s", "p99")
  local cnt_col = string.format("%3s", "n")
  return label_col .. " " .. last_col .. "  " .. p99_col .. " " .. cnt_col
end

local function current_label_width()
  if not PANEL then return MIN_LABEL_W end
  local rect = smelt.win.rect(PANEL.win)
  if not rect then return MIN_LABEL_W end
  local inner_w = math.max(rect.width - 2, 0) -- exclude 1-cell border each side
  local lw = inner_w - STATS_W
  if lw < MIN_LABEL_W then return MIN_LABEL_W end
  return lw
end

local function compose_lines(snap, label_w)
  local lines = { header_for(label_w) }
  local color_spans = {}
  local rows = snap.durations or {}
  local max_rows = PANEL_H - 3 -- border(2) + header(1)
  local n = math.min(#rows, max_rows)
  -- Use visual width (codepoints) for span offsets; fmt_us may emit µ.
  local width = smelt.text.width
  for i = 1, n do
    local r = rows[i]
    local last_s = fmt_us(r.last_us)
    local p99_s = fmt_us(r.p99_us)
    local cnt_s = string.format("%3d", math.min(r.count, 999))
    local line = pad_label(r.label, label_w) .. " " .. last_s .. "  " .. p99_s .. " " .. cnt_s
    lines[#lines + 1] = line
    local last_w = width(last_s)
    local p99_w = width(p99_s)
    local last_col = label_w + 1
    table.insert(color_spans, {
      row = i + 1,                       -- 1-based row index in buffer
      col = last_col,
      end_col = last_col + last_w,
      role = severity_role(r.last_us),
    })
    local p99_col = last_col + last_w + 2
    table.insert(color_spans, {
      row = i + 1,
      col = p99_col,
      end_col = p99_col + p99_w,
      role = severity_role(r.p99_us),
    })
  end
  if n == 0 then
    lines[#lines + 1] = "  (no samples yet)"
  end
  return lines, color_spans
end

local function paint_panel()
  if not PANEL then return end
  local ok, snap = pcall(smelt.metrics.perf.snapshot)
  if not ok then return end
  local label_w = current_label_width()
  local lines, spans = compose_lines(snap, label_w)
  smelt.buf.set_lines(PANEL.buf, lines)
  -- Clear and rewrite highlights each tick (set_lines wipes content but not extmarks).
  smelt.buf.clear_namespace(PANEL.buf, NS_HL)
  for _, sp in ipairs(spans) do
    smelt.buf.set_extmark(PANEL.buf, NS_HL, sp.row, sp.col, {
      end_col = sp.end_col,
      fg = sp.role,
    })
  end
end

local function open()
  if PANEL then return end
  smelt.metrics.perf.clear()
  smelt.metrics.perf.set_enabled(true)
  local buf = smelt.buf.create()
  local win = smelt.win.open(buf, { focusable = false })
  smelt.ui.overlay.open({
    title = {
      { text = " perf ", bold = true },
      { text = "(F12 to close) ", fg = "grey", dim = true },
    },
    anchor = "screen_at",
    corner = "ne",
    row    = 0,
    col    = 0,
    width  = PANEL_W,
    height = PANEL_H,
    border = { all = "Comment" },
    modal = false,
    blocks_agent = false,
    draggable = true,
    resizable = true,
    items = { { win = win, height = "fill" } },
  })
  local timer = smelt.timer.every(250, paint_panel)
  PANEL = { buf = buf, win = win, timer = timer }
  paint_panel()
end

local function close()
  if not PANEL then return end
  smelt.timer.cancel(PANEL.timer)
  smelt.win.close(PANEL.win)
  PANEL = nil
  smelt.metrics.perf.set_enabled(false)
  smelt.metrics.perf.clear()
end

local function toggle()
  if PANEL then close() else open() end
end

smelt.keymap.set("", "<F12>", toggle)

return M
