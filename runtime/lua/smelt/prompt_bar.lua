-- Prompt top + bottom bar windows.
--
-- Default `M.top_win` renders queued messages, a stash marker, and a
-- horizontal-rule bar with a left-aligned working indicator (traveling
-- wave) and a right-aligned model/tokens/cost group. `M.bottom_win`
-- renders a separator-only bar row.
--
-- Plugins can replace either window's renderer via
-- `prompt_bar.top_win:set_renderer(fn)` / `bottom_win:set_renderer(fn)`,
-- or replace the windows entirely from a custom `smelt.ui.layout.set`
-- composer.

local bar = require("smelt._bar")

local M = {}

local TOP_NS = smelt.ns("smelt.prompt_bar.top")
local BOT_NS = smelt.ns("smelt.prompt_bar.bottom")

-- ── helpers ─────────────────────────────────────────────────────────

local function queued_message_rows(queued, width)
  local rows = {}
  for _, msg in ipairs(queued) do
    -- Mirror prompt_buf::queued_message_rows: leading "  " indent, dim style.
    -- We don't wrap — the buffer line shows the message verbatim trimmed by
    -- the bar's available width.
    local text = "  " .. msg
    rows[#rows + 1] = {
      text = text,
      highlights = {
        {
          bytes_start = 0,
          bytes_end = #text,
          style = { fg = "Comment" },
        },
      },
    }
  end
  return rows
end

local function stash_row(width)
  local indent = "  "
  local label = "» Stashed (ctrl+s to unstash)"
  local text = indent .. label
  return {
    text = text,
    highlights = {
      {
        bytes_start = #indent,
        bytes_end = #text,
        style = { fg = "Comment" },
      },
    },
  }
end

-- ── working indicator (top bar left spans) ───────────────────────────
--
-- Reads work_* cells published by the engine. Returns a list of bar
-- spans suitable for `_bar.compose`. Each character of the indicator's
-- "glyph + label" gets its own span so the traveling wave can paint a
-- per-cell gradient.

local function indicator_spans()
  local state = smelt.cell("work_state"):get()
  if not state or state == "idle" then return nil end

  local label = smelt.cell("work_label"):get() or ""
  local elapsed_ms = smelt.cell("work_elapsed_ms"):get() or 0
  local retry_attempt = smelt.cell("work_retry_attempt"):get() or 0
  local retry_remaining_ms = smelt.cell("work_retry_remaining_ms"):get() or 0

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
  -- Leading `─` so the indicator sits one cell in from the edge.
  spans[#spans + 1] = {
    text = "\u{2500}",
    style = { fg = "SmeltBar" },
    priority = 0,
  }

  if active then
    local text
    if label == "" then
      text = " " .. glyph
    else
      text = " " .. glyph .. " " .. label
    end
    -- One span per Unicode codepoint so the wave paints per cell.
    local x = 0
    for _, codepoint in utf8.codes(text) do
      local ch = utf8.char(codepoint)
      local rgb = bar.wave_color_at(elapsed_ms, x)
      spans[#spans + 1] = {
        text = ch,
        style = { fg_rgb = rgb, bold = true },
        priority = 0,
      }
      x = x + 1
    end
  else
    local style
    if state == "paused" or state == "done" or state == "interrupted" then
      style = { fg = "Comment", dim = true }
    else
      style = { fg = "Normal", bold = true }
    end
    if glyph ~= "" then
      spans[#spans + 1] = { text = " " .. glyph, style = style, priority = 0 }
    end
    if label ~= "" then
      spans[#spans + 1] = { text = " " .. label, style = style, priority = 0 }
    end
  end

  -- Duration suppressed for `interrupted` (label alone reads cleaner).
  local secs = math.floor(elapsed_ms / 1000)
  if state ~= "interrupted" and secs > 0 then
    spans[#spans + 1] = {
      text = " " .. bar.format_duration(secs),
      style = { fg = "Comment", dim = true },
      priority = 1,
    }
  end

  if retry_attempt > 0 then
    spans[#spans + 1] = {
      text = string.format(" (retrying in %ds #%d)",
        math.floor(retry_remaining_ms / 1000), retry_attempt),
      style = { fg = "Comment", dim = true },
      priority = 2,
    }
  end

  return spans
end

-- ── right-side spans (model + reasoning + tokens + cost) ─────────────

local function reasoning_color_group(effort)
  if effort == "low" then return "SmeltReasonLow"
  elseif effort == "medium" then return "SmeltReasonMed"
  elseif effort == "high" then return "SmeltReasonHigh"
  elseif effort == "max" then return "SmeltReasonMax"
  else return "SmeltReasonOff"
  end
end

local function right_spans()
  local spans = {}
  local model = smelt.model()
  if model and model ~= "" then
    spans[#spans + 1] = {
      text = " " .. model,
      style = { fg = "Comment" },
      priority = 2,
    }
    local effort = smelt.reasoning()
    if effort and effort ~= "off" then
      spans[#spans + 1] = {
        text = " " .. effort,
        style = { fg = reasoning_color_group(effort) },
        priority = 2,
      }
    end
  end

  if smelt.settings.show_tokens then
    local ctx = smelt.session.context_tokens()
    if ctx then
      if #spans > 0 then
        spans[#spans + 1] = {
          text = " ·",
          style = { fg = "SmeltBar" },
          priority = 2,
        }
      end
      local window = smelt.session.context_window()
      local tok_text
      if window and window > 0 then
        local pct = math.floor(ctx / window * 100)
        tok_text = string.format(" %s (%d%%)", bar.format_tokens(ctx), pct)
      else
        tok_text = " " .. bar.format_tokens(ctx)
      end
      spans[#spans + 1] = {
        text = tok_text,
        style = { fg = "Comment" },
        priority = 1,
      }
    end
  end

  if smelt.settings.show_cost then
    local cost = smelt.session.cost()
    if cost and cost > 0 then
      if #spans > 0 then
        spans[#spans + 1] = {
          text = " ·",
          style = { fg = "SmeltBar" },
          priority = 2,
        }
      end
      spans[#spans + 1] = {
        text = " " .. bar.format_cost(cost),
        style = { fg = "Comment" },
        priority = 1,
      }
    end
  end

  return spans
end

-- ── renderers ───────────────────────────────────────────────────────

local function render_top(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  local rows = {}
  local queued = smelt.prompt.queued()
  for _, row in ipairs(queued_message_rows(queued, width)) do
    rows[#rows + 1] = row
  end
  if smelt.prompt.has_stash() then
    rows[#rows + 1] = stash_row(width)
  end
  rows[#rows + 1] = bar.compose(width, indicator_spans(), right_spans())
  bar.write_rows(buf, rows, TOP_NS)
end

local function render_bottom(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  bar.write_rows(buf, { bar.compose(width, nil, nil) }, BOT_NS)
end

-- ── window allocation ───────────────────────────────────────────────

M.top_win = smelt.win.new(smelt.buf.new({ name = "smelt.prompt_bar.top" }), {
  name = "smelt.prompt_bar.top",
  scrollbar = false,
  focusable = false,
  region = "prompt_above",
})
M.bottom_win = smelt.win.new(smelt.buf.new({ name = "smelt.prompt_bar.bottom" }), {
  name = "smelt.prompt_bar.bottom",
  scrollbar = false,
  focusable = false,
  region = "prompt_below",
})

if M.top_win then M.top_win:set_renderer(render_top) end
if M.bottom_win then M.bottom_win:set_renderer(render_bottom) end

-- Expose helper for the layout composer so it can compute the top bar's
-- row count from current state (queued messages, stash row, bar row).
function M.top_rows()
  local queued = smelt.prompt.queued()
  return 1 + #queued + (smelt.prompt.has_stash() and 1 or 0)
end

return M
