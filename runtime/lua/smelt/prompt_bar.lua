-- Prompt auxiliary + top + bottom bar windows.
--
-- Default `M.aux_win` owns the stable one-row gap between transcript and
-- prompt chrome. It renders either the discovery tip or a blank row;
-- notification overlays use the same row with higher z. `M.top_win` renders
-- queued messages, a stash marker, and a horizontal-rule bar. `M.bottom_win`
-- renders a horizontal-rule bar.
--
-- The rule row of the top and bottom windows is a bar (`smelt._bar`, rule
-- layout): a registry of named sources whose items sit in the rule, left items
-- from the left edge and right items from the right edge. The built-in sources
-- (`pause`, `indicator`, `elapsed`, `retry` on the left; `model`, `tokens`,
-- `cost` on the right) are registered below. Plugins add, replace or remove
-- sources with `smelt.prompt_bar.top.add` / `.remove` and the matching
-- `smelt.prompt_bar.bottom` functions. To change more than the rule, replace a
-- window's renderer with `aux_win:set_renderer(fn)`, `top_win:set_renderer(fn)`
-- or `bottom_win:set_renderer(fn)`, or replace the windows entirely from a
-- custom `smelt.ui.layout.set` composer.

local bar = require("smelt._bar")
local tips = require("smelt.tips")

local M = {}

local AUX_NS = smelt.ns("smelt.prompt_bar.aux")
local TOP_NS = smelt.ns("smelt.prompt_bar.top")
local BOT_NS = smelt.ns("smelt.prompt_bar.bottom")
local TOKEN_PRIORITY = 0
local INDICATOR_PRIORITY = 1
local LABEL_PRIORITY = 2
local SECONDARY_PRIORITY = 3
local OPTIONAL_PRIORITY = 4
local COMMENT_DIM_STYLE = { fg = "Comment", dim = true }
local renderer_subscriptions = {}

local function invalidate_on(win, names)
  if not win or type(smelt.signal.subscribe) ~= "function" then return end
  for _, name in ipairs(names) do
    renderer_subscriptions[#renderer_subscriptions + 1] = smelt.signal.subscribe(name, function()
      win:invalidate_renderer()
    end)
  end
end

local function invalidate_when_changed(win, name, display_value)
  if not win or type(smelt.signal.subscribe) ~= "function" then return end
  renderer_subscriptions[#renderer_subscriptions + 1] = smelt.signal.subscribe(
    name,
    function(value, previous)
      if display_value(value) ~= display_value(previous) then
        win:invalidate_renderer()
      end
    end)
end

-- ── helpers ─────────────────────────────────────────────────────────

local function queued_message_row(row, width)
  width = math.max(width or 0, 0)
  local kind = row.kind or "turn"
  local marker = kind == "request" and "»" or "›"
  local prefix = "  " .. marker .. " "
  -- Each queued message owns one row. Normalize preview whitespace before
  -- measuring cells so line breaks and tabs cannot displace the prompt chrome.
  local preview = (row.text or ""):gsub("%s+", " "):gsub("^ ", ""):gsub(" $", "")
  local text, body_end = bar.truncate_right_padded(prefix .. preview, width)
  local prefix_end = math.min(#prefix, body_end)
  return {
    text = text,
    highlights = {
      {
        bytes_start = 0,
        bytes_end = prefix_end,
        style = { fg = "Comment" },
        selectable = false,
      },
      {
        bytes_start = prefix_end,
        bytes_end = body_end,
        style = { fg = "Comment" },
      },
    },
  }
end

local function stash_row(width)
  width = math.max(width or 0, 0)
  local indent = "  "
  local label = "◌ Stashed (ctrl+s to unstash)"
  local text, body_end = bar.truncate_right_padded(indent .. label, width)
  return {
    text = text,
    highlights = {
      {
        bytes_start = math.min(#indent, body_end),
        bytes_end = body_end,
        style = { fg = "Comment" },
      },
    },
  }
end

local function more_row(hidden, width)
  width = math.max(width or 0, 0)
  local text, body_end = bar.truncate_right_padded(
    "  +" .. hidden .. " more queued", width)
  return {
    text = text,
    highlights = {
      {
        bytes_start = 0,
        bytes_end = body_end,
        style = { fg = "Comment", dim = true },
      },
    },
  }
end

-- Baseline for whether the aux row may show the tip: empty prompt unless a
-- modal picker owns it, idle, no stash/queue/notification, and tips enabled.
-- The row itself is always reserved by the layout so tips do not shift content.
local function tip_eligible(queued)
  if not tips.enabled() then return false end
  if #queued > 0 or smelt.prompt.has_stash() then return false end
  if (smelt.prompt.text() or "") ~= "" and not smelt.prompt.is_modal() then return false end
  if smelt.signal.get("notification_visible") then return false end
  local work_state = smelt.signal.get("work_state")
  if work_state and work_state ~= "idle" then return false end
  return true
end

local function should_show_tip(queued)
  return tip_eligible(queued)
end

local function tip_row(width)
  width = math.max(width or 0, 0)
  local tip = tips.prompt_tip()
  if not tip then return nil end
  local prefix = "  tip "
  local key = tip.key or ""
  local separator = key ~= "" and ", " or ""
  local body = key .. separator .. (tip.text or "")
  local text, body_end = bar.truncate_right_padded(prefix .. body, width)
  local prefix_end = math.min(#prefix, body_end)
  local key_end = math.min(prefix_end + #key, body_end)
  local desc_start = math.min(key_end + #separator, body_end)
  local highlights = {
    {
      bytes_start = 0,
      bytes_end = prefix_end,
      style = { fg = "SmeltAccent", bold = true },
      selectable = false,
    },
  }
  if key ~= "" then
    highlights[#highlights + 1] = {
      bytes_start = prefix_end,
      bytes_end = key_end,
      style = { fg = "Comment" },
    }
    highlights[#highlights + 1] = {
      bytes_start = key_end,
      bytes_end = desc_start,
      style = { fg = "Comment", dim = true },
      selectable = false,
    }
  end
  highlights[#highlights + 1] = {
    bytes_start = desc_start,
    bytes_end = body_end,
    style = { fg = "Comment" },
  }
  return { text = text, highlights = highlights }
end

-- ── bars ────────────────────────────────────────────────────────────

local function resize_style(position)
  if not smelt.signal.get("prompt_resize_active") then return nil end
  local chrome = smelt.signal.get("prompt_resize_chrome") or ""
  if chrome == position or chrome == "both" then
    return { hl_group = "SmeltResizeHandle" }
  end
  return nil
end

local top_bar = bar.new({
  name = "smelt.prompt_bar.top",
  layout = "rule",
  bar_style = function() return resize_style("top") end,
})
local bottom_bar = bar.new({
  name = "smelt.prompt_bar.bottom",
  layout = "rule",
  bar_style = function() return resize_style("bottom") end,
})

M.top = {}
M.bottom = {}
smelt.prompt_bar = M

local builtin = {}

--- The built-in source registered under `name` (`pause`, `indicator`,
--- `elapsed`, `retry`, `model`, `tokens` or `cost`), or nil. A replacement
--- source can call it to keep the default for the cases it does not draw.
---@tier ui_host
---@type fun(name: string): (fun(): smelt.bar.Item[]?)?
function smelt.prompt_bar.top.builtin(name)
  return builtin[name]
end

-- Repaint after plugin-owned state changes that are not represented by a
-- built-in signal. Adding, replacing, and removing sources invalidates
-- automatically.
---@tier ui_host
---@type fun()
function smelt.prompt_bar.top.invalidate()
  bar.invalidate(top_bar)
end

--- Add a named source of items to the rule of the bar above the prompt.
--- `handler` runs on every repaint and returns a list of items (see
--- `smelt.bar.Item`); errors are reported on stderr and skip the source. The
--- built-in sources are `pause`, `indicator`, `elapsed`, `retry`, `model`,
--- `tokens` and `cost`; adding one of those names replaces it in place, keeping
--- its position. Returns a `Reg` that removes this source, unless a later `add`
--- with the same name has replaced it.
---@tier ui_host
---@type fun(name: string, handler: fun(): smelt.bar.Item[]?): smelt.Reg
function smelt.prompt_bar.top.add(name, handler)
  return bar.add(top_bar, name, handler)
end

--- Remove every source named `name` from the bar above the prompt.
---@tier ui_host
---@type fun(name: string)
function smelt.prompt_bar.top.remove(name)
  bar.remove(top_bar, name)
end

-- Repaint after plugin-owned state changes that are not represented by a
-- built-in signal. Adding, replacing, and removing sources invalidates
-- automatically.
---@tier ui_host
---@type fun()
function smelt.prompt_bar.bottom.invalidate()
  bar.invalidate(bottom_bar)
end

--- Add a named source of items to the rule of the bar below the prompt. It has
--- no built-in sources, so the rule stays plain until a plugin adds one. Same
--- contract as `smelt.prompt_bar.top.add`.
---@tier ui_host
---@type fun(name: string, handler: fun(): smelt.bar.Item[]?): smelt.Reg
function smelt.prompt_bar.bottom.add(name, handler)
  return bar.add(bottom_bar, name, handler)
end

--- Remove every source named `name` from the bar below the prompt.
---@tier ui_host
---@type fun(name: string)
function smelt.prompt_bar.bottom.remove(name)
  bar.remove(bottom_bar, name)
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

builtin.pause = function()
  local pause = smelt.signal.get("auto_continue_status")
  if not pause then return nil end
  local reason = pause.kind == "quota" and "quota exceeded"
    or pause.kind == "rate_limited" and "rate limited" or "interrupted"
  local action = "paused"
  if pause.phase == "waiting_for_idle" then
    action = "resuming when idle"
  elseif pause.phase == "scheduled" then
    local at = math.floor(pause.next_attempt_at_ms / 1000)
    local today = smelt.time.format(math.floor(smelt.time.now_ms() / 1000), "%Y-%m-%d")
    local format = smelt.time.format(at, "%Y-%m-%d") == today and "%H:%M" or "%b %d %H:%M"
    local time = smelt.time.format(at, format)
    if time then action = "resuming at " .. time end
  end
  return {
    { text = reason, style = COMMENT_DIM_STYLE, priority = INDICATOR_PRIORITY, separated = true },
    { text = action, style = COMMENT_DIM_STYLE, priority = LABEL_PRIORITY, separated = true },
  }
end

-- One coal says what the turn is doing. It catches when the turn starts or
-- resumes and then breathes. It heats with tool calls: each call bumps it, it
-- keeps warming for as long as a call is pending, and it cools back to a dull
-- red once nothing is running. While a failed request waits to retry it is
-- banked low and warms as the wait runs down. A dialog that pauses the turn
-- leaves it still and dim in its color. A finished turn cools it to gray over a
-- second, and an interrupted one, which you asked for, goes gray at once. The
-- label types in white-hot whenever it changes, rests in gray, and a hammer
-- sweep runs along it on repeat.
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
local TYPE_MS = 45
local FIRST_SWEEP_MS = 1600
local SWEEP_MS = 2200
local SWEEP_STEP_MS = 110
local FLARE_COOL_MS = 650
local PAUSED_LEVEL = 0.4

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

local function coal_item(color)
  return {
    runs = { { text = COAL, style = { fg = color, bold = true } } },
    priority = INDICATOR_PRIORITY,
    selectable = false,
  }
end

local function breath(now)
  return 0.45 + 0.55 * (0.5 + 0.5 * math.sin(now / BREATH_MS * 2 * math.pi))
end

-- The coal catches when the turn starts or resumes, then breathes.
local started
local function working_coal(now)
  started = started or now
  local p = math.min((now - started) / IGNITE_MS, 1)
  return coal_item(mix(ground(), heat(), 0.1 + (breath(now) - 0.1) * p))
end

-- A failed request waits out a backoff before the next attempt: the coal is
-- banked low and warms as the wait runs down.
local backoff
local function banked_coal()
  local remaining = smelt.signal.get("work_retry_remaining_ms") or 0
  backoff = math.max(backoff or 0, remaining)
  local progress = backoff > 0 and 1 - remaining / backoff or 0
  return coal_item(mix(ground(), heat(), 0.15 + 0.45 * progress))
end

-- A finished turn cools to gray; the timer repaints it, since nothing else
-- animates once the turn is over.
local done_at, cooling
smelt.signal.subscribe("work_state", function(state)
  if state == "done" or state == "interrupted" or state == "idle" then
    settle(smelt.time.now_ms())
    pending_calls = 0
  end
  if state ~= "done" then
    done_at = nil
    return
  end
  done_at = smelt.time.now_ms()
  if cooling then cooling:remove() end
  cooling = smelt.timer.every(50, function() bar.invalidate(top_bar) end)
  smelt.timer.set(COOL_MS + 100, function()
    cooling:remove()
    cooling = nil
  end)
end)

-- How far letter `i` is flared toward white, from 0 (resting gray) to 1: it
-- lands white-hot as it types, and a sweep runs along the word on repeat.
local function flare(i, age)
  local flared = math.exp(-math.max(age - (i - 1) * TYPE_MS, 0) / FLARE_COOL_MS)
  local reach = age - FIRST_SWEEP_MS - (i - 1) * SWEEP_STEP_MS
  if reach >= 0 then
    flared = math.max(flared, math.exp(-(reach % SWEEP_MS) / FLARE_COOL_MS))
  end
  return flared
end

local shown, shown_since
local function typed_label(label, now)
  if label ~= shown then shown, shown_since = label, now end
  local light = smelt.theme.is_light()
  local rest = light and 120 or 150
  local rest_color = { rest, rest, rest }
  local flash = light and { 40, 12, 0 } or { 255, 252, 240 }
  local age = now - shown_since
  local typed = math.min(#label, math.floor(age / TYPE_MS))
  local runs = {}
  for i = 1, #label + 1 do
    if i <= typed or i == #label + 1 then
      local landed = i <= typed or age >= #label * TYPE_MS
      runs[#runs + 1] = {
        text = i <= #label and label:sub(i, i) or "\u{2026}",
        style = { fg = mix(rest_color, flash, landed and flare(i, age) or 0), bold = true },
      }
    end
  end
  return { runs = runs, priority = LABEL_PRIORITY }
end

builtin.indicator = function()
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
  local now = smelt.time.now_ms()

  if state == "working" or state == "busy" or state == "retrying" then
    local mark
    if state == "retrying" then
      started = nil
      mark = banked_coal()
    else
      backoff = nil
      mark = working_coal(now)
    end
    return { mark, typed_label(label, now) }
  end

  started, backoff, shown = nil, nil, nil
  local mark
  if state == "paused" then
    mark = coal_item(mix(ground(), heat(), PAUSED_LEVEL))
  elseif state == "done" then
    local cool = done_at and math.min((now - done_at) / COOL_MS, 1) or 1
    mark = coal_item(mix(heat(), ash(), cool))
  else
    mark = coal_item(ash())
  end
  local items = { mark }
  if label ~= "" then
    items[#items + 1] = { text = label, style = COMMENT_DIM_STYLE, priority = LABEL_PRIORITY }
  end
  return items
end

-- Duration suppressed for `interrupted` (label alone reads cleaner).
builtin.elapsed = function()
  local state = work_state()
  if not state or state == "interrupted" then return nil end
  local secs = math.floor((smelt.signal.get("work_elapsed_ms") or 0) / 1000)
  if secs <= 0 then return nil end
  return {
    text = smelt.text.format_duration(secs),
    style = COMMENT_DIM_STYLE,
    priority = SECONDARY_PRIORITY,
  }
end

builtin.retry = function()
  if not work_state() then return nil end
  local attempt = smelt.signal.get("work_retry_attempt") or 0
  if attempt <= 0 then return nil end
  local remaining_ms = smelt.signal.get("work_retry_remaining_ms") or 0
  return {
    text = string.format("(retrying in %ds #%d)", math.max(1, math.ceil(remaining_ms / 1000)), attempt),
    style = COMMENT_DIM_STYLE,
    priority = OPTIONAL_PRIORITY,
  }
end

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

builtin.model = function()
  local status = session_status()
  local model = status.model or smelt.model.current()
  if not model or model == "" then return nil end
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
end

builtin.tokens = function()
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
end

builtin.cost = function()
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
end

for _, name in ipairs({ "pause", "indicator", "elapsed", "retry", "model", "tokens", "cost" }) do
  bar.add(top_bar, name, builtin[name])
end

-- ── renderers ───────────────────────────────────────────────────────

local function render_top(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  local rows = {}
  local queued = smelt.prompt.queued_rows()

  -- The layout composer already capped our window height; trim from the
  -- oldest queued messages inward when the queue does not all fit.
  local win_height = (win:rect() or {}).height
  -- During the very first frame a renderer can run before the window has
  -- a resolved rect; in that case use the natural row count as a fallback.
  local available_rows = win_height or M.top_rows()
  -- Custom layouts may provide only one row; the indicator takes priority.
  local reserved = math.min(M.min_top_rows(), available_rows)
  local show_stash = reserved > 1
  local queue_slots = math.max(0, available_rows - reserved)
  local visible_queued = math.min(#queued, queue_slots)
  local hidden = #queued - visible_queued
  local show_more = hidden > 0 and queue_slots > 0
  if show_more then
    -- The summary row occupies one of the queue slots, so keep one fewer
    -- concrete queued message when collapsing. Otherwise the indicator bar is
    -- pushed out of the capped top window on short terminals.
    visible_queued = queue_slots - 1
    hidden = #queued - visible_queued
  end
  local start_idx = #queued - visible_queued + 1
  for i = start_idx, #queued do
    rows[#rows + 1] = queued_message_row(queued[i], width)
  end
  if show_more then
    rows[#rows + 1] = more_row(hidden, width)
  end
  if show_stash then
    rows[#rows + 1] = stash_row(width)
  end
  rows[#rows + 1] = bar.row(top_bar, width)
  bar.write_rows(buf, rows, TOP_NS)
end

local function render_aux(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  local queued = smelt.prompt.queued_rows()
  local row = nil
  if should_show_tip(queued) then row = tip_row(width) end
  local blank = { text = string.rep(" ", math.max(width or 0, 0)), highlights = {} }
  bar.write_rows(buf, { row or blank }, AUX_NS)
end

local function render_bottom(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  bar.write_rows(buf, { bar.row(bottom_bar, width) }, BOT_NS)
end

-- ── window allocation ───────────────────────────────────────────────

M.aux_win = smelt.win.new(smelt.buf.new({ name = "smelt.prompt_bar.aux" }), {
  name = "smelt.prompt_bar.aux",
  scrollbar = false,
  surface = "selectable_text",
  region = "prompt_aux",
})
M.top_win = smelt.win.new(smelt.buf.new({ name = "smelt.prompt_bar.top" }), {
  name = "smelt.prompt_bar.top",
  scrollbar = false,
  surface = "selectable_text",
  region = "prompt_above",
})
M.bottom_win = smelt.win.new(smelt.buf.new({ name = "smelt.prompt_bar.bottom" }), {
  name = "smelt.prompt_bar.bottom",
  scrollbar = false,
  surface = "inert",
  region = "prompt_below",
})

top_bar.win = M.top_win
bottom_bar.win = M.bottom_win

if M.aux_win then
  M.aux_win:set_renderer(render_aux)
end
if M.top_win then
  M.top_win:set_renderer(render_top)
end
if M.bottom_win then
  M.bottom_win:set_renderer(render_bottom)
end

invalidate_on(M.aux_win, {
  "input_epoch",
  "notification_visible",
  "prompt_queue_revision",
  "work_state",
})
invalidate_on(M.top_win, {
  "auto_continue_status",
  "fast_mode",
  "input_epoch",
  "model",
  "prompt_queue_revision",
  "prompt_resize_active",
  "prompt_resize_chrome",
  "reasoning",
  "session_epoch",
  "tokens_used",
  "work_elapsed_ms",
  "work_label",
  "work_retry_attempt",
  "work_state",
})
invalidate_when_changed(M.top_win, "work_retry_remaining_ms", function(value)
  return math.max(1, math.ceil((tonumber(value) or 0) / 1000))
end)
invalidate_on(M.bottom_win, {
  "prompt_resize_active",
  "prompt_resize_chrome",
})

-- Mandatory chrome, excluding queued messages that may be collapsed.
function M.min_top_rows()
  return smelt.prompt.has_stash() and 2 or 1
end

-- Expose helper for the layout composer so it can compute the top bar's
-- row count from current state (queued messages, stash row, bar row).
-- `max_top_rows` is an optional cap; when omitted the natural row count is
-- returned. The default layout composer passes a cap that preserves at
-- least two transcript rows.
function M.top_rows(max_top_rows)
  local queued = smelt.prompt.queued()
  local rows = M.min_top_rows() + #queued
  if type(max_top_rows) == "number" then
    return math.min(rows, math.max(1, max_top_rows))
  end
  return rows
end

if smelt.win and smelt.win.PROMPT then
  local previous_top_rows = M.top_rows()
  renderer_subscriptions[#renderer_subscriptions + 1] = smelt.win.PROMPT:on(
    "text_changed",
    function()
      if M.aux_win then M.aux_win:invalidate_renderer() end
      if M.top_win then M.top_win:invalidate_renderer() end
      local top_rows = M.top_rows()
      if top_rows ~= previous_top_rows then
        previous_top_rows = top_rows
        smelt.ui.layout.invalidate()
      end
    end)
end

return M
