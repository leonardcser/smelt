-- `smelt.transcript.pending`: a strip bar under the transcript for blocks that
-- wait to join it. A note that arrives during a turn joins the transcript at
-- the next request boundary; until then it shows here, dimmed.
--
-- Each source summarizes one category. Nonempty rows stack in display order:
-- process completions, monitor events, then agent messages. This summary
-- does not change the order in which notes join history. Add a source for
-- another kind with `smelt.transcript.pending:add_source(name, source)` and
-- read its blocks with `smelt.transcript.pending_blocks()`.

local bar = require("smelt._bar")
local builtins = require("smelt.transcript.builtins")

local signals = { "transcript_pending_revision" }

-- One item uses its header; several use the committed group's summary.
local function source(kind, row, color, summary)
  return { signals = signals, render = function()
    local blocks = {}
    for _, block in ipairs(smelt.transcript.pending_blocks()) do
      local category = block.event == "monitor_event" and "monitor_event" or block.kind
      if category == kind then blocks[#blocks + 1] = block end
    end
    local first = blocks[1]
    if not first then return nil end
    local text, hl = first.text, first.hl_group or color
    if #blocks > 1 then
      local failed
      text, failed = summary(blocks)
      hl = failed and "ErrorMsg" or color
    elseif first.agent_status == "failed" or first.agent_status == "blocked" then
      hl = "ErrorMsg"
    end
    return { text = text, style = { fg = hl, italic = true, dim = true }, truncatable = true, row = row }
  end }
end

--- The strip of blocks that wait to join the transcript. It takes no rows
--- while nothing is pending. Its built-in sources are `process_status`,
--- `monitor_event`, and `agent_message`, one row each when nonempty.
---@tier ui_host
---@type smelt.bar.Bar
smelt.transcript.pending = bar.new({
  name = "smelt.transcript.pending",
  min_rows = 0,
  sources = {
    { "process_status", source("process_status", 1, "SmeltProcess", builtins.background_process_summary) },
    { "monitor_event", source("monitor_event", 2, "SmeltProcess", builtins.monitor_event_summary) },
    { "agent_message", source("agent_message", 3, "SmeltAgent", builtins.agent_message_summary) },
  },
  win_opts = { pad_left = 0, pad_right = 1, focus_on_click = smelt.transcript.win },
})

return smelt.transcript.pending
