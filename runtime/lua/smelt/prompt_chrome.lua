-- The chrome around the prompt input, top to bottom:
--
--   * `smelt.prompt.aux`: a strip bar between the transcript and the prompt
--     block, one row by default. Its `tip` source shows a discovery tip while
--     the prompt is idle and empty; notifications float over the same row.
--   * `smelt.prompt.queue`: a strip bar with one row per queued message
--     (`queued`) and the stash marker (`stash`).
--   * `smelt.prompt.top`: a rule bar with the working indicator (`pause`,
--     `indicator`, `elapsed`, `retry`) on the left and `model`, `tokens` and
--     `cost` on the right. Dragging it resizes the prompt.
--   * `smelt.prompt.win`: the input itself.
--   * `smelt.prompt.bottom`: a rule bar with no sources.
--
-- Each bar is a `smelt.bar.Bar`: add, replace or remove sources, change its
-- look with `configure`, or swap it for any node with `set_view`.
-- `smelt.prompt.node(opts)` stacks the five for a layout composer.

local bar = require("smelt._bar")
local tips = require("smelt.tips")

local TOKEN_PRIORITY = 0
local INDICATOR_PRIORITY = 1
local LABEL_PRIORITY = 2
local SECONDARY_PRIORITY = 3
local OPTIONAL_PRIORITY = 4
local COMMENT_DIM_STYLE = { fg = "Comment", dim = true }
local subscriptions = {}
local input_signals = { "input_epoch", "prompt_queue_revision" }
local tip_signals = { "input_epoch", "notification_visible", "prompt_queue_revision", "prompt_stashed", "work_state" }

-- ── queue ───────────────────────────────────────────────────────────

-- The last two rows of the queue, below every queued message.
local MORE_ROW, STASH_ROW = -2, -1

local function stash_source()
  if not smelt.prompt.has_stash() then return nil end
  return { text = "◌ Stashed (ctrl+s to unstash)", style = { fg = "Comment" }, row = STASH_ROW }
end

-- One row per queued message, newest last. When the layout gives the queue
-- fewer rows than it wants, the oldest messages fold into a "+N more" row.
local function queued_source(ctx)
  local queued = smelt.prompt.queued_rows()
  local room = ctx.rows or #queued
  local shown = math.min(#queued, room)
  local hidden = #queued - shown
  if hidden > 0 and room > 0 then
    shown = room - 1
    hidden = #queued - shown
  end
  local items = {}
  for i = #queued - shown + 1, #queued do
    local row = queued[i]
    local marker = row.kind == "request" and "»" or "›"
    -- Each queued message owns one row. Normalize preview whitespace so line
    -- breaks and tabs cannot displace the prompt chrome.
    local preview = (row.text or ""):gsub("%s+", " "):gsub("^ ", ""):gsub(" $", "")
    items[#items + 1] = {
      runs = {
        { text = marker .. " ", style = { fg = "Comment" }, selectable = false },
        { text = preview, style = { fg = "Comment" } },
      },
      row = i,
    }
  end
  if hidden > 0 and room > 0 then
    items[#items + 1] = { text = "+" .. hidden .. " more queued", style = COMMENT_DIM_STYLE, row = MORE_ROW }
  end
  return items
end

--- Queued messages and the stash marker, between the auxiliary bar and the
--- bar above the prompt. Built-in sources: `queued` (one row per message,
--- folding the oldest into "+N more" when rows run short) and `stash`, which
--- keeps the last row.
---@tier ui_host
---@type smelt.bar.Bar
smelt.prompt.queue = bar.new({
  name = "smelt.prompt.queue",
  min_rows = 0,
  -- The stash goes first so it keeps its row when the queue folds.
  sources = {
    { "stash", { render = stash_source, signals = { "prompt_stashed" } } },
    { "queued", { render = queued_source, signals = input_signals } },
  },
  win_opts = { pad_left = 2, pad_right = 2, focus_on_click = smelt.prompt.win },
})

-- ── auxiliary bar ───────────────────────────────────────────────────

-- The tip shows while the prompt is empty (unless a modal picker owns it),
-- idle, with no stash, queue or notification, and tips enabled.
local function tip_source()
  if not tips.enabled() then return nil end
  if smelt.prompt.has_stash() or smelt.prompt.queue:rows() > 0 then return nil end
  if (smelt.prompt.text() or "") ~= "" and not smelt.prompt.is_modal() then return nil end
  if smelt.signal.get("notification_visible") then return nil end
  local work_state = smelt.signal.get("work_state")
  if work_state and work_state ~= "idle" then return nil end
  local tip = tips.prompt_tip()
  if not tip then return nil end
  local runs = { { text = "tip ", style = { fg = "SmeltAccent", bold = true }, selectable = false } }
  if (tip.key or "") ~= "" then
    runs[#runs + 1] = { text = tip.key, style = { fg = "Comment" } }
    runs[#runs + 1] = { text = ", ", style = COMMENT_DIM_STYLE, selectable = false }
  end
  runs[#runs + 1] = { text = tip.text or "", style = { fg = "Comment" } }
  return { runs = runs }
end

--- The strip between the transcript and the prompt block. It keeps one row
--- even when empty so tips and notifications do not shift the transcript. Its
--- built-in source is `tip`.
---@tier ui_host
---@type smelt.bar.Bar
smelt.prompt.aux = bar.new({
  name = "smelt.prompt.aux",
  sources = { { "tip", { render = tip_source, signals = tip_signals } } },
  win_opts = { pad_left = 2, pad_right = 2, focus_on_click = smelt.prompt.win },
})

-- ── built-in sources of the top bar ─────────────────────────────────

local top_sources = {}
local function seconds(value) return math.floor((tonumber(value) or 0) / 1000) end
local function retry_seconds(value) return math.max(1, math.ceil((tonumber(value) or 0) / 1000)) end
local function define(name, signals, render)
  top_sources[#top_sources + 1] = { name, { render = render, signals = signals } }
end

-- ── built-in sources: working indicator (left) ───────────────────────
--
-- Read the work_* signals published by the engine. While the agent works the
-- glyph and label share one traveling wave, one cell per character.

-- The live work state, or nil when idle or while the auto-continue pause
-- message owns the left side.
local function work_state()
  if smelt.signal.get("auto_continue_status") then return nil end
  local state = smelt.signal.get("work_state")
  if not state or state == "idle" then return nil end
  return state
end

-- One coal says what the turn is doing. It catches when the turn starts or
-- resumes and then breathes. It heats with tool calls: each call bumps it, it
-- keeps warming for as long as a call is pending, and it cools back to a dull
-- red once nothing is running. While a failed request waits to retry it is
-- banked low and warms as the wait runs down. A dialog that pauses the turn
-- leaves it still and dim in its color. A finished turn cools it to gray over a
-- second, and an interrupted one, which you asked for, goes gray at once. The
-- label types in white-hot whenever it changes, rests in gray, and a hammer
-- sweep runs along it on repeat. Motion that repeats (the breath, the sweep and
-- the frame grid) is a function of the wall clock, so every smelt instance
-- shows the same frame at the same moment. Motion that happens once (typing,
-- ignition, heat) follows its own events. Every letter step lasts a whole
-- number of frames, so no step stays on screen longer than the others. Once the
-- coal is still, no frame is asked for.
local COAL = "\u{25CF}"
local MIN_HEAT = 0.05
local CALL_BUMP = 0.15
local RISE_SECS = 8
local HEAT_FADE_MS = 8000
local HOT_STOPS = { { 120, 30, 20 }, { 230, 90, 20 }, { 255, 190, 60 }, { 255, 245, 220 } }
local COLD_STOPS = { { 150, 60, 40 }, { 190, 80, 20 }, { 170, 100, 10 }, { 110, 60, 10 } }
local BREATH_MS = 1600
local IGNITE_MS = 450
local COOL_MS = 1000
local TYPE_MS = 44
local SWEEP_MS = 2200
local SWEEP_STEP_MS = 110
local FLARE_COOL_MS = 650
local PAUSED_LEVEL = 0.4
local FRAME_MS = 44

local function mix(a, b, t)
  t = math.max(0, math.min(t, 1))
  return {
    math.floor(a[1] + (b[1] - a[1]) * t),
    math.floor(a[2] + (b[2] - a[2]) * t),
    math.floor(a[3] + (b[3] - a[3]) * t),
  }
end

local function ground() return smelt.theme.is_light() and { 252, 251, 249 } or { 23, 23, 28 } end
-- The gray a finished coal settles to. The cool-down ends on exactly this color.
local function ash() return smelt.theme.is_light() and { 168, 164, 158 } or { 96, 96, 108 } end

-- How hot the coal burns, from 0 to 1. A bump from each tool call is folded in
-- when the call starts or ends, and heat then rises while a call is pending and
-- decays when none is.
local heat_level, heat_at, pending_calls = MIN_HEAT, 0, 0

local function warmth(now)
  if pending_calls > 0 then
    return math.min(heat_level + (now - heat_at) / 1000 / RISE_SECS, 1)
  end
  return MIN_HEAT + (heat_level - MIN_HEAT) * math.exp(-(now - heat_at) / HEAT_FADE_MS)
end

local function settle(now)
  heat_level, heat_at = warmth(now), now
end

smelt.events.on("turn_start", function()
  heat_level, heat_at, pending_calls = MIN_HEAT, smelt.time.now_ms(), 0
end)

smelt.events.on("tool_start", function()
  local now = smelt.time.now_ms()
  settle(now)
  heat_level = math.min(heat_level + CALL_BUMP, 1)
  pending_calls = pending_calls + 1
end)

smelt.events.on("tool_end", function()
  settle(smelt.time.now_ms())
  pending_calls = math.max(pending_calls - 1, 0)
end)

local function heat()
  local stops = smelt.theme.is_light() and COLD_STOPS or HOT_STOPS
  local at = warmth(smelt.time.now_ms()) * (#stops - 1)
  local i = math.min(math.floor(at) + 1, #stops - 1)
  return mix(stops[i], stops[i + 1], at - (i - 1))
end

-- Time to the next frame on the wall-clock grid shared by every instance.
local function next_frame(now) return FRAME_MS - now % FRAME_MS end

-- The coal in `color_at(now)`. With `animating`, it is recolored on every frame
-- while `animating(now)` holds, without laying the bar out again.
local function coal_item(color_at, animating)
  return {
    runs = { { text = COAL, style = { fg = color_at(smelt.time.now_ms()), bold = true } } },
    priority = INDICATOR_PRIORITY,
    selectable = false,
    paint = animating and function(slice, ctx)
      local now = smelt.time.now_ms()
      slice:patch_style(0, 0, 1, 1, { fg = color_at(now) })
      if animating(now) then ctx.invalidate_after(next_frame(now)) end
    end,
  }
end

local function breath(now)
  return 0.45 + 0.55 * (0.5 + 0.5 * math.sin(now / BREATH_MS * 2 * math.pi))
end

local function always() return true end

-- The coal catches when the turn starts or resumes, then breathes.
local started
local function working_coal()
  return coal_item(function(now)
    started = started or now
    local p = math.min((now - started) / IGNITE_MS, 1)
    return mix(ground(), heat(), 0.1 + (breath(now) - 0.1) * p)
  end, always)
end

-- A failed request waits out a backoff before the next attempt: the coal is
-- banked low and warms as the wait runs down.
local backoff
local function banked_coal()
  return coal_item(function()
    local remaining = smelt.signal.get("work_retry_remaining_ms") or 0
    backoff = math.max(backoff or 0, remaining)
    local progress = backoff > 0 and 1 - remaining / backoff or 0
    return mix(ground(), heat(), 0.15 + 0.45 * progress)
  end, always)
end

local done_at
smelt.signal.subscribe("work_state", function(state)
  if state == "done" or state == "interrupted" or state == "idle" then
    settle(smelt.time.now_ms())
    pending_calls = 0
  end
  done_at = state == "done" and smelt.time.now_ms() or nil
end)

-- How far letter `i` is flared toward white, from 0 (resting gray) to 1: it
-- lands white-hot as it types, and a sweep runs along the word on repeat,
-- locked to the wall clock `now`.
local function flare(i, age, now)
  local landed = math.exp(-math.max(age - (i - 1) * TYPE_MS, 0) / FLARE_COOL_MS)
  local swept = math.exp(-((now - (i - 1) * SWEEP_STEP_MS) % SWEEP_MS) / FLARE_COOL_MS)
  return math.max(landed, swept)
end

local function rest_color()
  local rest = smelt.theme.is_light() and 120 or 150
  return { rest, rest, rest }
end

-- The label types in, followed by an ellipsis. Each typed letter lays the bar
-- out again; the colors change on every frame without a layout.
local shown, shown_since
local function typed_label(label, ctx)
  local now = smelt.time.now_ms()
  if label ~= shown then shown, shown_since = label, now end
  local letters = {}
  for _, code in utf8.codes(label) do letters[#letters + 1] = utf8.char(code) end
  local count = #letters
  local age = now - shown_since
  local typed = math.min(count, math.floor(age / TYPE_MS))
  if typed < count then ctx.invalidate_after(TYPE_MS - age % TYPE_MS) end
  local text = table.concat(letters, "", 1, typed) .. "\u{2026}"
  return {
    runs = { { text = text, style = { fg = rest_color(), bold = true } } },
    priority = LABEL_PRIORITY,
    paint = function(slice, paint_ctx)
      local now = smelt.time.now_ms()
      local flash = smelt.theme.is_light() and { 40, 12, 0 } or { 255, 252, 240 }
      local rest = rest_color()
      local age = now - shown_since
      local x = 0
      for i = 1, typed + 1 do
        local letter = i <= typed and letters[i] or "\u{2026}"
        local width = smelt.text.width(letter)
        local landed = i <= typed or age >= count * TYPE_MS
        slice:patch_style(x, 0, width, 1, { fg = mix(rest, flash, landed and flare(i, age, now) or 0) })
        x = x + width
      end
      paint_ctx.invalidate_after(next_frame(now))
    end,
  }
end

-- A turn paused by a limit keeps its dim coal. The text says why it waits and
-- when it resumes.
define("pause", { "auto_continue_status" }, function()
  local pause = smelt.signal.get("auto_continue_status")
  if not pause then return nil end
  local reason = pause.kind == "quota" and "quota exceeded"
    or pause.kind == "rate_limited" and "rate limited" or "interrupted"
  local action = "paused"
  if pause.phase == "resuming" then
    action = "resuming now"
  elseif pause.phase == "waiting_for_idle" then
    action = "resuming when idle"
  elseif pause.phase == "scheduled" then
    local at = math.floor(pause.next_attempt_at_ms / 1000)
    local today = smelt.time.format(math.floor(smelt.time.now_ms() / 1000), "%Y-%m-%d")
    local format = smelt.time.format(at, "%Y-%m-%d") == today and "%H:%M" or "%b %d %H:%M"
    local time = smelt.time.format(at, format)
    if time then action = "resuming at " .. time end
  end
  return {
    coal_item(function() return mix(ground(), heat(), PAUSED_LEVEL) end),
    { text = reason, style = COMMENT_DIM_STYLE, priority = INDICATOR_PRIORITY, separated = true },
    { text = action, style = COMMENT_DIM_STYLE, priority = LABEL_PRIORITY, separated = true },
  }
end)

define("indicator", { "auto_continue_status", "work_state", "work_label" }, function(ctx)
  local state = work_state()
  if not state then return nil end

  local label = smelt.signal.get("work_label") or ""
  if label == "" then
    if state == "done" then label = "done"
    elseif state == "interrupted" then label = "interrupted"
    elseif state == "paused" then label = "paused"
    elseif state == "retrying" or state == "working" or state == "busy" then label = "working"
    end
  end

  if state == "working" or state == "busy" or state == "retrying" then
    local mark
    if state == "retrying" then
      started = nil
      mark = banked_coal()
    else
      backoff = nil
      mark = working_coal()
    end
    return { mark, typed_label(label, ctx) }
  end

  started, backoff, shown = nil, nil, nil
  local mark
  if state == "paused" then
    mark = coal_item(function() return mix(ground(), heat(), PAUSED_LEVEL) end)
  elseif state == "done" then
    local function cool(now) return done_at and math.min((now - done_at) / COOL_MS, 1) or 1 end
    mark = coal_item(function(now) return mix(heat(), ash(), cool(now)) end, function(now) return cool(now) < 1 end)
  else
    mark = coal_item(ash)
  end
  local items = { mark }
  if label ~= "" then
    items[#items + 1] = { text = label, style = COMMENT_DIM_STYLE, priority = LABEL_PRIORITY }
  end
  return items
end)

-- Duration suppressed for `interrupted` (label alone reads cleaner).
define("elapsed", { "auto_continue_status", "work_state", { "work_elapsed_ms", seconds } }, function(ctx)
  local state = work_state()
  if not state or state == "interrupted" then return nil end
  local elapsed_ms = smelt.signal.get("work_elapsed_ms") or 0
  if state == "working" or state == "busy" or state == "retrying" then
    ctx.invalidate_after(1000 - elapsed_ms % 1000)
  end
  local secs = math.floor(elapsed_ms / 1000)
  if secs <= 0 then return nil end
  return {
    text = smelt.text.format_duration(secs),
    style = COMMENT_DIM_STYLE,
    priority = SECONDARY_PRIORITY,
  }
end)

define("retry", { "auto_continue_status", "work_state", "work_retry_attempt", { "work_retry_remaining_ms", retry_seconds } }, function(ctx)
  if not work_state() then return nil end
  local attempt = smelt.signal.get("work_retry_attempt") or 0
  if attempt <= 0 then return nil end
  local remaining_ms = smelt.signal.get("work_retry_remaining_ms") or 0
  if remaining_ms > 0 then ctx.invalidate_after((remaining_ms - 1) % 1000 + 1) end
  return {
    text = string.format("(retrying in %ds #%d)", math.max(1, math.ceil(remaining_ms / 1000)), attempt),
    style = COMMENT_DIM_STYLE,
    priority = OPTIONAL_PRIORITY,
  }
end)

-- ── built-in sources: model, tokens and cost (right) ─────────────────

local function reasoning_color_group(effort)
  if effort == "low" then return "SmeltReasonLow"
  elseif effort == "medium" then return "SmeltReasonMedium"
  elseif effort == "high" then return "SmeltReasonHigh"
  elseif effort == "xhigh" or effort == "max" then return "SmeltReasonMax"
  elseif effort == "ultra" then return "SmeltReasonUltra"
  else return "Comment"
  end
end

local function session_status()
  return smelt.session.status and smelt.session.status() or {}
end

-- Name the model the provider says it runs, so an alias shows its version:
-- `claude-code/haiku` reads `claude-code/haiku-5-5`. A leading word the
-- provider name already says is dropped: `claude-code/claude-opus-4-8` reads
-- `claude-code/opus-4-8`.
local function model_label(key, resolved)
  local provider = key:match("^([^/]+)/")
  if not provider or not resolved or resolved == "" then return key end
  local word = provider:match("^[^-]+") .. "-"
  if resolved:sub(1, #word) == word then resolved = resolved:sub(#word + 1) end
  return provider .. "/" .. resolved
end

define("model", { "model", "reasoning", "fast_mode", "session_epoch", "work_state", "tokens_used" }, function()
  local status = session_status()
  local model = status.model or smelt.model.current()
  if not model or model == "" then return nil end
  model = model_label(model, status.resolved)
  local items = {}
  if status.fast and status.fast.active then
    items[#items + 1] = {
      text = ">>",
      style = { fg = "Comment", bold = true },
      priority = SECONDARY_PRIORITY,
      align_right = true,
    }
  end
  items[#items + 1] = {
    text = model,
    style = { fg = "Comment" },
    priority = SECONDARY_PRIORITY,
    align_right = true,
    separated = true,
  }
  local reasoning = status.reasoning or {}
  local effort = reasoning.effort or smelt.reasoning.current()
  if effort and effort ~= "off" then
    items[#items + 1] = {
      text = effort .. (reasoning.marker or ""),
      style = { fg = reasoning_color_group(effort) },
      priority = SECONDARY_PRIORITY,
      align_right = true,
    }
  end
  return items
end)

define("tokens", { "tokens_used", "session_epoch", "model", "work_state" }, function()
  if not smelt.settings.show_tokens then return nil end
  local context = session_status().context or {}
  local ctx = context.tokens
  if ctx == nil then
    ctx = smelt.session.context_tokens()
  end
  if not ctx then return nil end
  local window = context.window or smelt.session.context_window()
  local stale_mark = context.marker or ""
  local text
  if window and window > 0 then
    local pct = math.floor(ctx / window * 100)
    text = string.format("%s%s (%d%%)", smelt.text.format_tokens(ctx), stale_mark, pct)
  else
    text = smelt.text.format_tokens(ctx) .. stale_mark
  end
  return {
    text = text,
    style = { fg = "Comment" },
    priority = TOKEN_PRIORITY,
    align_right = true,
    separated = true,
  }
end)

define("cost", { "tokens_used", "session_epoch", "work_state" }, function()
  if not smelt.settings.show_cost then return nil end
  local cost = smelt.session.cost()
  if not cost or cost <= 0 then return nil end
  return {
    text = smelt.text.format_cost(cost),
    style = { fg = "Comment" },
    priority = OPTIONAL_PRIORITY,
    align_right = true,
    separated = true,
  }
end)

-- ── top and bottom bars ─────────────────────────────────────────────

--- The bar right above the prompt input, a rule by default. Built-in sources:
--- `pause`, `indicator`, `elapsed` and `retry` on the left, `model`, `tokens`
--- and `cost` on the right. Dragging it resizes the prompt.
---@tier ui_host
---@type smelt.bar.Bar
smelt.prompt.top = bar.new({
  name = "smelt.prompt.top",
  layout = "rule",
  sources = top_sources,
  win_opts = { focus_on_click = smelt.prompt.win },
})
local top = smelt.prompt.top
if top.win then smelt.prompt.set_resize_handle(top.win) end

--- The bar right below the prompt input, a plain rule with no sources by
--- default.
---@tier ui_host
---@type smelt.bar.Bar
smelt.prompt.bottom = bar.new({
  name = "smelt.prompt.bottom",
  layout = "rule",
  win_opts = { surface = "inert", focus_on_click = smelt.prompt.win },
})

-- ── invalidation ────────────────────────────────────────────────────

local aux, queue, bottom = smelt.prompt.aux, smelt.prompt.queue, smelt.prompt.bottom
local function invalidate_aux() aux:invalidate() end
local function invalidate_queue() queue:invalidate() end
-- The bar that resizes the prompt lights up while it is dragged.
subscriptions[#subscriptions + 1] = smelt.signal.subscribe("prompt_resize_active", function(active)
  top:highlight(active and { hl_group = "SmeltResizeHandle" } or nil)
end)
subscriptions[#subscriptions + 1] = smelt.prompt.win:on("text_changed", function()
  invalidate_aux()
  invalidate_queue()
end)

-- ── layout ──────────────────────────────────────────────────────────

--- The prompt block as one layout node: the auxiliary bar, the queue, the top
--- bar, the input and the bottom bar, stacked. Returns the node and its rows.
--- `opts.input_rows` is the input's wanted height (default 1). With
--- `opts.max_rows`, the block fits in that many rows: the input shrinks first
--- (to one row), then the queue folds, keeping at least one row.
---@tier ui_host
---@type fun(opts?: { input_rows?: integer, max_rows?: integer }): smelt.ui.layout, integer
function smelt.prompt.node(opts)
  opts = opts or {}
  local aux_node, aux_rows = aux:node()
  local queue_node, queue_rows = queue:node()
  local top_node, top_rows = top:node()
  local bottom_node, bottom_rows = bottom:node()
  local fixed = aux_rows + top_rows + bottom_rows
  local input_rows = opts.input_rows or 1
  if opts.max_rows then
    local queue_min = math.min(queue_rows, 1)
    input_rows = math.min(input_rows, math.max(1, opts.max_rows - fixed - queue_min))
    queue_rows = math.min(queue_rows, math.max(queue_min, opts.max_rows - fixed - input_rows))
  end
  local layout = smelt.ui.layout
  local node = layout.vbox({
    { height = aux_rows, aux_node },
    { height = queue_rows, queue_node },
    { height = top_rows, top_node },
    { height = input_rows, layout.leaf(smelt.prompt.win) },
    { height = bottom_rows, bottom_node },
  })
  return node, aux_rows + queue_rows + top_rows + input_rows + bottom_rows
end
