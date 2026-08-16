-- Prompt auxiliary + top + bottom bar windows.
--
-- Default `M.aux_win` owns the stable one-row gap between transcript and
-- prompt chrome. It renders either the discovery tip or a blank row;
-- notification overlays use the same row with higher z. `M.top_win` renders
-- queued messages, a stash marker, and a horizontal-rule bar with a
-- left-aligned working indicator (traveling wave) and a right-aligned
-- model/tokens/cost group. `M.bottom_win` renders a separator-only bar row.
--
-- Plugins can replace any window's renderer via
-- `prompt_bar.aux_win:set_renderer(fn)`, `top_win:set_renderer(fn)` /
-- `bottom_win:set_renderer(fn)`, or replace the windows entirely from a
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

-- ── helpers ─────────────────────────────────────────────────────────

local function queued_message_row(row, width)
  width = math.max(width or 0, 0)
  local kind = row.kind or "turn"
  local marker = kind == "request" and "»" or "›"
  local prefix = "  " .. marker .. " "
  local text, body_end = bar.truncate_right_padded(prefix .. (row.text or ""), width)
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

-- ── working indicator (top bar left spans) ───────────────────────────
--
-- Reads work_* signals published by the engine. Returns a list of bar
-- spans suitable for `_bar.compose`. Each character of the indicator's
-- "glyph + label" gets its own span so the traveling wave can paint a
-- per-cell gradient.

local function indicator_spans(opts)
  opts = opts or {}
  local bar_style = opts.bar_style or { fg = "SmeltSeparator" }
  local state = smelt.signal.get("work_state")
  if not state or state == "idle" then return nil end

  local label = smelt.signal.get("work_label") or ""
  local elapsed_ms = smelt.signal.get("work_elapsed_ms") or 0
  local retry_attempt = smelt.signal.get("work_retry_attempt") or 0
  local retry_remaining_ms = smelt.signal.get("work_retry_remaining_ms") or 0

  if label == "" then
    if state == "done" then label = "done"
    elseif state == "interrupted" then label = "interrupted"
    elseif state == "paused" then label = "paused"
    end
  end

  local active = state == "working" or state == "retrying" or state == "busy"
  if active and label ~= "" then label = label .. "\u{2026}" end

  local glyph
  if active or state == "paused" then
    glyph = smelt.spinner.glyph()
  else
    glyph = ""
  end

  local spans = {}
  local wave_x = 0
  local function add_wave_text(text, priority, selectable_start)
    for _, codepoint in utf8.codes(text) do
      local ch = utf8.char(codepoint)
      local rgb = smelt.spinner.wave_color_at(wave_x)
      spans[#spans + 1] = {
        text = ch,
        style = { fg = rgb, bold = true },
        priority = priority,
        selectable = selectable_start and wave_x >= selectable_start,
      }
      wave_x = wave_x + 1
    end
  end

  -- Leading `─` so the indicator sits one cell in from the edge. It drops with
  -- the compact spinner so narrow bars fall back to a clean rule instead of an
  -- orphan edge cell.
  spans[#spans + 1] = {
    text = "\u{2500}",
    style = bar_style,
    priority = INDICATOR_PRIORITY,
    selectable = false,
  }

  if active then
    if glyph ~= "" then add_wave_text(" " .. glyph, INDICATOR_PRIORITY, nil) end
    if label ~= "" then add_wave_text(" " .. label, LABEL_PRIORITY, 3) end
  else
    local style
    if state == "paused" or state == "done" or state == "interrupted" then
      style = { fg = "Comment", dim = true }
    else
      style = { fg = "Normal", bold = true }
    end
    if glyph ~= "" then
      spans[#spans + 1] = {
        text = " " .. glyph,
        style = style,
        priority = INDICATOR_PRIORITY,
        selectable = false,
      }
    end
    if label ~= "" then
      spans[#spans + 1] = {
        text = " " .. label,
        style = style,
        priority = LABEL_PRIORITY,
      }
    end
  end

  -- Duration suppressed for `interrupted` (label alone reads cleaner).
  local secs = math.floor(elapsed_ms / 1000)
  if state ~= "interrupted" and secs > 0 then
    spans[#spans + 1] = {
      text = " " .. smelt.text.format_duration(secs),
      style = { fg = "Comment", dim = true },
      priority = SECONDARY_PRIORITY,
    }
  end

  if retry_attempt > 0 then
    local retry_secs = math.max(1, math.ceil(retry_remaining_ms / 1000))
    spans[#spans + 1] = {
      text = string.format(" (retrying in %ds #%d)", retry_secs, retry_attempt),
      style = { fg = "Comment", dim = true },
      priority = OPTIONAL_PRIORITY,
    }
  end

  return spans
end

-- ── right-side spans (model + reasoning + tokens + cost) ─────────────

local function reasoning_color_group(effort)
  if effort == "low" then return "SmeltReasonLow"
  elseif effort == "medium" then return "SmeltReasonMedium"
  elseif effort == "high" then return "SmeltReasonHigh"
  elseif effort == "max" then return "SmeltReasonMax"
  else return "Comment"
  end
end

local function right_spans(opts)
  opts = opts or {}
  local bar_style = opts.bar_style or { fg = "SmeltSeparator" }
  local spans = {}
  local status = smelt.session.status and smelt.session.status() or {}
  local model = status.model or smelt.model.current()
  if model and model ~= "" then
    local fast_active = status.fast and status.fast.active
    if fast_active then
      spans[#spans + 1] = {
        text = " >>",
        style = { fg = "Comment", bold = true },
        priority = SECONDARY_PRIORITY,
      }
    end
    spans[#spans + 1] = {
      text = " " .. model,
      style = { fg = "Comment" },
      priority = SECONDARY_PRIORITY,
    }
    local reasoning = status.reasoning or {}
    local effort = reasoning.effort or smelt.reasoning.current()
    if effort and effort ~= "off" then
      spans[#spans + 1] = {
        text = " " .. effort .. (reasoning.marker or ""),
        style = { fg = reasoning_color_group(effort) },
        priority = SECONDARY_PRIORITY,
      }
    end
  end

  if smelt.settings.show_tokens then
    local context = status.context or {}
    local ctx = context.tokens
    if ctx == nil then
      ctx = smelt.session.context_tokens()
    end
    if ctx then
      if #spans > 0 then
        spans[#spans + 1] = {
          text = " ·",
          style = bar_style,
          priority = SECONDARY_PRIORITY,
          selectable = false,
        }
      end
      local window = context.window or smelt.session.context_window()
      local stale_mark = context.marker or ""
      local tok_text
      if window and window > 0 then
        local pct = math.floor(ctx / window * 100)
        tok_text = string.format(" %s%s (%d%%)", smelt.text.format_tokens(ctx), stale_mark, pct)
      else
        tok_text = " " .. smelt.text.format_tokens(ctx) .. stale_mark
      end
      spans[#spans + 1] = {
        text = tok_text,
        style = { fg = "Comment" },
        priority = TOKEN_PRIORITY,
      }
    end
  end

  if smelt.settings.show_cost then
    local cost = smelt.session.cost()
    if cost and cost > 0 then
      if #spans > 0 then
        spans[#spans + 1] = {
          text = " ·",
          style = bar_style,
          priority = OPTIONAL_PRIORITY,
          selectable = false,
        }
      end
      spans[#spans + 1] = {
        text = " " .. smelt.text.format_cost(cost),
        style = { fg = "Comment" },
        priority = OPTIONAL_PRIORITY,
      }
    end
  end

  return spans
end

local function resize_bar_opts(position)
  if not smelt.signal.get("prompt_resize_active") then return nil end
  local chrome = smelt.signal.get("prompt_resize_chrome") or ""
  if chrome == position or chrome == "both" then
    return { bar_style = { hl_group = "SmeltResizeHandle" } }
  end
  return nil
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
  local natural_rows = M.top_rows()
  local reserved = 1 -- indicator bar row
  if smelt.prompt.has_stash() then reserved = reserved + 1 end
  local queue_slots = math.max(0, (win_height or natural_rows) - reserved)
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
  if smelt.prompt.has_stash() then
    rows[#rows + 1] = stash_row(width)
  end
  local bar_opts = resize_bar_opts("top")
  rows[#rows + 1] = bar.compose(width, indicator_spans(bar_opts), right_spans(bar_opts), bar_opts)
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
  bar.write_rows(buf, { bar.compose(width, nil, nil, resize_bar_opts("bottom")) }, BOT_NS)
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

if M.aux_win then
  M.aux_win:set_renderer(render_aux)
end
if M.top_win then
  M.top_win:set_renderer(render_top)
end
if M.bottom_win then
  M.bottom_win:set_renderer(render_bottom)
end

-- Expose helper for the layout composer so it can compute the top bar's
-- row count from current state (queued messages, stash row, bar row).
-- `max_top_rows` is an optional cap; when omitted the natural row count is
-- returned. The default layout composer passes a cap that preserves at
-- least two transcript rows.
function M.top_rows(max_top_rows)
  local queued = smelt.prompt.queued()
  local rows = 1 + #queued
  if smelt.prompt.has_stash() then rows = rows + 1 end
  if type(max_top_rows) == "number" then
    return math.min(rows, math.max(1, max_top_rows))
  end
  return rows
end

return M
