-- Clickable scroll-pill overlays navigate the transcript.
--
--   * Bottom pill - " ↓ jump to bottom " while scrolled off-tail; click re-pins to tail.
--   * Top pill    - first line of the nearest actionable user message;
--     click reveals it below the pill, then the next target walks back.
--     The pill's row is the transcript's top inset, so block jumps and
--     reveals land below it.
-- Disable via `smelt.builtins.disable({ plugins = { "scroll_pills" } })`.

local pills = smelt.transcript.pills
-- Keep the first jump clear even when it makes a hidden top bar appear.
smelt.transcript.top_inset(1)

local function height(kind)
  local bar = pills[kind]
  if not bar.overlay then return 1 end
  if not bar.view_factory then return bar:rows() end
  local rect = bar.overlay and bar.overlay:rect()
  return rect and rect.height or 1
end

local function present(kind, view, label, target)
  local bar = pills[kind]
  bar:_present({ label = label, target = target, view = view },
    math.max(0, view.viewport.width - (kind == "top" and 1 or 0)), {
      name = bar.name, anchor = "win", target = view.window,
      attach = kind == "top" and "nw" or "s", reserve = kind == "top", z = 5,
      modal = false, blocks_agent = false, border = "none",
    })
end

local function cursor_viewport_row(view)
  return view.focused and view.cursor and view.cursor.viewport_row or nil
end

local function cursor_under_bottom_pill(view)
  local row = cursor_viewport_row(view)
  return row ~= nil and row >= view.viewport.height - height("bottom")
end

-- ── Top pill: "jump to previous user message" ─────────────────────────

local function user_block_for_top_pill(view)
  if view.viewport.height <= 0 or not view.viewport.scrollable or view.viewport.at_top then return nil end
  local block = view:previous_block({ role = "user" })
  if not block or block.first_line == "" then return nil end
  return block
end

local function refresh(view)
  local viewport = view.viewport
  if viewport.height > 0 and viewport.scrollable
    and not viewport.following_tail and not viewport.at_bottom and not cursor_under_bottom_pill(view) then
    present("bottom", view, " ↓ jump to bottom ")
    if cursor_under_bottom_pill(view) then pills.bottom:_dismiss() end
  else
    pills.bottom:_dismiss()
  end
  local target = user_block_for_top_pill(view)
  local row = cursor_viewport_row(view)
  if viewport.width > 1 and target and (row == nil or row >= height("top")) then
    present("top", view, target.first_line, target)
    if row ~= nil and row < height("top") then pills.top:_dismiss() end
  else
    pills.top:_dismiss()
  end
end

-- A committed view includes projection, geometry, navigation, focus, and cursor
-- state, so the plugin never has to join unrelated event streams.
smelt.transcript.watch_view(refresh)

-- Submitting starts a new turn at the semantic transcript tail.
smelt.events.on("input_submit", function()
  smelt.transcript.follow_tail()
end)
