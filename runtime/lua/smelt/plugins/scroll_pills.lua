-- Scroll-pill overlays for transcript navigation:
--   * Bottom pill - " ↓ jump to bottom " while scrolled off-tail; click re-pins to tail.
--   * Top pill    - first line of the nearest user message above the viewport;
--     click reveals it with one row of gap so repeated clicks walk back.
-- Disable via `smelt.builtins.disable({ plugins = { "scroll_pills" } })`.

local ns_bottom = smelt.ns("smelt.scroll_pills.bottom")
local ns_top = smelt.ns("smelt.scroll_pills.top")

local PILL_BG = "SmeltScrollPillBg"
local PILL_FG = "Comment"
local TOP_PILL_ROW = 0

local state = {
  transcript_win = nil,
  bottom_overlay = nil,
  bottom_buf = nil,
  bottom_win = nil,
  top_overlay = nil,
  top_buf = nil,
  top_win = nil,
  top_width = nil,
  top_target_idx = nil,
}

-- ── Common lifecycle ───────────────────────────────────────────────────

local function close_bottom()
  if state.bottom_overlay then state.bottom_overlay:close() end
  state.bottom_overlay = nil
  state.bottom_buf = nil
  state.bottom_win = nil
end

local function close_top()
  if state.top_overlay then state.top_overlay:close() end
  state.top_overlay = nil
  state.top_buf = nil
  state.top_win = nil
  state.top_width = nil
  state.top_target_idx = nil
end

local function close_all()
  close_bottom()
  close_top()
end

local function should_show_bottom(scroll)
  return scroll
    and scroll.viewport
    and scroll.viewport > 0
    and scroll.overflow
    and not scroll.follow
    and not scroll.at_bottom
end

local function can_show_top(scroll)
  return scroll
    and scroll.viewport
    and scroll.viewport > 0
    and scroll.overflow
end

-- ── Bottom pill: "jump to bottom" ─────────────────────────────────────

local BOTTOM_LABEL = " ↓ jump to bottom "
local BOTTOM_WIDTH = smelt.text.width(BOTTOM_LABEL)

local function open_bottom()
  if state.bottom_overlay then return end
  local buf = smelt.buf.new({ name = "smelt.scroll_pills.bottom.buf" })
  buf:lines({ BOTTOM_LABEL })
  buf:clear_ns(ns_bottom)
  buf:mark(ns_bottom, 1, 0, {
    end_col = #BOTTOM_LABEL,
    fg = PILL_FG,
    bg = PILL_BG,
    bold = true,
  })
  local win = smelt.win.new(buf, {
    name = "smelt.scroll_pills.bottom.win",
    surface = "inert",
    scrollbar = false,
  })
  win:on("press", function()
    if state.transcript_win then
      state.transcript_win:scroll("tail")
      close_all()
    end
  end)
  state.bottom_buf = buf
  state.bottom_win = win
  state.bottom_overlay = smelt.overlay.new({
    name = "smelt.scroll_pills.bottom",
    anchor = "win",
    target = state.transcript_win,
    attach = "s",
    z = 5,
    modal = false,
    blocks_agent = false,
    border = "none",
    layout = smelt.ui.layout.leaf(win, { measure = { BOTTOM_WIDTH, 1 } }),
  })
end

local function transcript_cursor_rows(scroll)
  if smelt.focus() ~= "transcript" or not state.transcript_win then return nil, nil end
  local row = state.transcript_win:cursor()
  local rect = state.transcript_win:rect()
  if row == nil or scroll == nil or rect == nil or not scroll.top or not scroll.viewport then return nil, nil end
  local screen_row = row - scroll.top
  if screen_row < 0 or screen_row >= scroll.viewport then return nil, nil end
  return screen_row, rect.row + screen_row
end

local function cursor_under_bottom_pill(scroll)
  local screen_row = transcript_cursor_rows(scroll)
  return screen_row ~= nil and scroll and scroll.viewport and screen_row == scroll.viewport - 1
end

-- ── Top pill: "jump to last user message" ─────────────────────────────

-- Most-recent user block at-or-above the viewport top. Hidden when that
-- block sits exactly at the viewport top (already visible, click would no-op).
local function user_block_for_top_pill(scroll)
  if not can_show_top(scroll) then return nil end
  local blocks = smelt.transcript.blocks()
  for i = #blocks, 1, -1 do
    local b = blocks[i]
    if b.role == "user" and b.first_line ~= "" and b.first_row <= scroll.top then
      if b.first_row == scroll.top then return nil end
      return b
    end
  end
  return nil
end

local function open_top(width)
  local buf = smelt.buf.new({ name = "smelt.scroll_pills.top.buf" })
  buf:lines({ "" })
  local win = smelt.win.new(buf, {
    name = "smelt.scroll_pills.top.win",
    surface = "inert",
    scrollbar = false,
  })
  win:on("press", function()
    if state.top_target_idx and state.transcript_win then
      local blocks = smelt.transcript.blocks()
      for _, b in ipairs(blocks) do
        if b.idx == state.top_target_idx then
          state.transcript_win:reveal(b.first_row, { top_padding = 1, cursor = true })
          return
        end
      end
    end
  end)
  state.top_buf = buf
  state.top_win = win
  state.top_width = width
  state.top_overlay = smelt.overlay.new({
    name = "smelt.scroll_pills.top",
    anchor = "screen_at",
    corner = "nw",
    row = TOP_PILL_ROW,
    col = 0,
    z = 5,
    modal = false,
    blocks_agent = false,
    border = "none",
    layout = smelt.ui.layout.leaf(win, { measure = { width, 1 } }),
  })
end

local function paint_top_row(width, label)
  local inner = smelt.text.fit(label, math.max(0, width - 2), { suffix = "…" })
  local row = " " .. inner .. " "
  state.top_buf:lines({ row })
  state.top_buf:clear_ns(ns_top)
  state.top_buf:mark(ns_top, 1, 0, {
    end_col = #row,
    fg = PILL_FG,
    bg = PILL_BG,
    bold = true,
    hl_eol = true,
  })
end

local function cursor_under_top_pill(scroll)
  local _, terminal_row = transcript_cursor_rows(scroll)
  return terminal_row == TOP_PILL_ROW
end

local function refresh_top(scroll)
  local rect = state.transcript_win:rect()
  -- Leave the transcript's scrollbar column uncovered.
  local width = rect and math.max(0, rect.width - 1) or 0
  if width <= 0 then close_top(); return end

  local target = user_block_for_top_pill(scroll)
  if not target or cursor_under_top_pill(scroll) then close_top(); return end

  if state.top_overlay and state.top_width ~= width then close_top() end
  if not state.top_overlay then open_top(width) end
  state.top_target_idx = target.idx
  paint_top_row(width, target.first_line)
end

-- Reconcile overlays from the current transcript viewport. Every event path
-- funnels through here so reset, resize, scrollbar drag, wheel scroll, and
-- selection autoscroll cannot leave stale pills behind.
local function refresh()
  if not state.transcript_win then
    close_all()
    return
  end

  local scroll = state.transcript_win:scroll()
  if should_show_bottom(scroll) and not cursor_under_bottom_pill(scroll) then
    open_bottom()
  else
    close_bottom()
  end
  refresh_top(scroll)
end

-- ── React to view/session changes ──────────────────────────────────────
-- UiHost-bound; re-wires on every `/reload`.

smelt.lifecycle.on_ready(function()
  close_all()
  state.transcript_win = smelt.win.transcript()
  state.transcript_win:on("scrolled", refresh)
  state.transcript_win:on("resized", refresh)
  state.transcript_win:on("focus", refresh)
  state.transcript_win:on("blur", refresh)
  refresh()
end)

-- The session reset path can clear transcript content without changing the
-- previous scroll tuple enough to emit `scrolled`; lifecycle/history cells are
-- the semantic source of truth for stale overlay cleanup.
smelt.cell("history"):subscribe(function(payload)
  if payload and payload.kind == "cleared" then close_all() end
end)
smelt.cell("session_started"):subscribe(close_all)

smelt.cell("cursor_pos"):subscribe(function()
  if smelt.focus() == "transcript" then refresh() end
end)

-- Jump to bottom when the user submits a message so the new turn is visible.
smelt.cell("input_submit"):subscribe(function()
  if state.transcript_win then
    state.transcript_win:scroll("tail")
  end
  close_all()
end)
