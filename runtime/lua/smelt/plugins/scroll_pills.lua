-- Scroll-pill overlays for transcript navigation:
--   * Bottom pill - " ↓ jump to bottom " while scrolled off-tail; click re-pins to tail.
--   * Top pill    - first line of the nearest actionable user message;
--     click aligns it to the viewport top, then the next target walks back.
-- Disable via `smelt.builtins.disable({ plugins = { "scroll_pills" } })`.

local ns_bottom = smelt.ns("smelt.scroll_pills.bottom")
local ns_top = smelt.ns("smelt.scroll_pills.top")

local PILL_BG = "SmeltScrollPillBg"
local PILL_FG = "Comment"

local state = {
  transcript_win = nil,
  bottom_overlay = nil,
  bottom_buf = nil,
  bottom_win = nil,
  top_overlay = nil,
  top_buf = nil,
  top_win = nil,
  top_width = nil,
  top_target = nil,
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
  state.top_target = nil
end

local function should_show_bottom(view)
  return view.viewport.height > 0
    and view.viewport.scrollable
    and not view.viewport.at_bottom
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
    smelt.transcript.follow_tail()
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

local function cursor_viewport_row(view)
  return view.focused and view.cursor and view.cursor.viewport_row or nil
end

local function cursor_under_bottom_pill(view)
  local row = cursor_viewport_row(view)
  return row ~= nil and row == view.viewport.height - 1
end

-- ── Top pill: "jump to previous user message" ─────────────────────────

local function user_block_for_top_pill(view)
  if view.viewport.height <= 0 or not view.viewport.scrollable or view.viewport.at_top then return nil end
  local block = view:previous_block({ role = "user" })
  if not block or block.first_line == "" then return nil end
  return block
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
    if state.top_target then
      smelt.transcript.reveal(state.top_target, { align = "top", move_cursor = true })
    end
  end)
  state.top_buf = buf
  state.top_win = win
  state.top_width = width
  state.top_overlay = smelt.overlay.new({
    name = "smelt.scroll_pills.top",
    anchor = "win",
    target = state.transcript_win,
    attach = "nw",
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

local function cursor_under_top_pill(view)
  return cursor_viewport_row(view) == 0
end

local function refresh_top(view)
  -- Leave the transcript's scrollbar column uncovered.
  local width = math.max(0, view.viewport.width - 1)
  if width <= 0 then close_top(); return end

  local target = user_block_for_top_pill(view)
  if not target or cursor_under_top_pill(view) then close_top(); return end

  if state.top_overlay and state.top_width ~= width then close_top() end
  if not state.top_overlay then open_top(width) end
  state.top_target = target
  paint_top_row(width, target.first_line)
end

local function refresh(view)
  state.transcript_win = view.window
  if should_show_bottom(view) and not cursor_under_bottom_pill(view) then
    open_bottom()
  else
    close_bottom()
  end
  refresh_top(view)
end

-- A committed view includes projection, geometry, navigation, focus, and cursor
-- state, so the plugin never has to join unrelated event streams.
smelt.transcript.watch_view(refresh)

-- Submitting starts a new turn at the semantic transcript tail.
smelt.events.on("input_submit", function()
  smelt.transcript.follow_tail()
end)
