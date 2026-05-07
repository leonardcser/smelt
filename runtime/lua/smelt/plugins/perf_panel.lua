-- F12 perf panel.
--
-- Small overlay anchored to the screen's top-right corner showing live
-- duration percentiles for the busiest perf labels. Non-modal +
-- non-focusable so the prompt keeps input focus while it's open.
-- F12 again to dismiss.

local M = {}

local PANEL = nil
local NS_HL = smelt.buf.create_namespace("smelt.perf_panel")

local PANEL_W = 38
local PANEL_H = 14
local LABEL_W = 14

-- Severity thresholds in microseconds. Maps a duration into a theme
-- role name; the buffer extmark resolves the role to a colour.
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

local function pad_label(label)
  if #label > LABEL_W then
    return label:sub(1, LABEL_W - 1) .. "…"
  end
  return label .. string.rep(" ", LABEL_W - #label)
end

local function compose_lines(snap)
  local lines = { "label          last      p99   n" }
  local color_spans = {}                -- {row, col_start, col_end, role}
  local rows = snap.durations or {}
  local max_rows = PANEL_H - 3          -- border (2) + header (1)
  local n = math.min(#rows, max_rows)
  for i = 1, n do
    local r = rows[i]
    local last_s = fmt_us(r.last_us)
    local p99_s = fmt_us(r.p99_us)
    local cnt_s = string.format("%3d", math.min(r.count, 999))
    local line = pad_label(r.label) .. " " .. last_s .. "  " .. p99_s .. " " .. cnt_s
    lines[#lines + 1] = line
    -- Colour the `last` and `p99` cells by their own severity.
    local last_col = LABEL_W + 1
    table.insert(color_spans, {
      row = i + 1,                       -- 1-based row index in buffer
      col = last_col,
      end_col = last_col + #last_s,
      role = severity_role(r.last_us),
    })
    local p99_col = last_col + #last_s + 2
    table.insert(color_spans, {
      row = i + 1,
      col = p99_col,
      end_col = p99_col + #p99_s,
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
  local ok, snap = pcall(smelt.metrics.perf_snapshot)
  if not ok then return end
  local lines, spans = compose_lines(snap)
  smelt.buf.set_lines(PANEL.buf, lines)
  -- Repaint highlights — set_lines wipes rendered content but extmarks
  -- in a dedicated namespace persist; clear and rewrite each tick.
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
  smelt.metrics.perf_clear()
  smelt.metrics.perf_set_enabled(true)
  local buf = smelt.buf.create()
  local win = smelt.win.open(buf, { focusable = false })
  smelt.ui.overlay.open({
    title = " perf ",
    placement = "screen_at",
    corner = "ne",
    row = 0,
    col = 0,
    width = PANEL_W,
    height = PANEL_H,
    modal = false,
    blocks_agent = false,
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
  smelt.metrics.perf_set_enabled(false)
  smelt.metrics.perf_clear()
end

local function toggle()
  if PANEL then close() else open() end
end

smelt.keymap.set("", "<F12>", toggle)

return M
