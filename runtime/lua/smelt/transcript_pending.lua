-- `smelt.transcript.pending`: a strip bar under the transcript for blocks that
-- wait to join it. A note that arrives during a turn joins the transcript at
-- the next request boundary; until then it shows here, dimmed.
--
-- Each block kind is one source on its own row, and a source groups all of
-- its pending blocks into that row. The rows stack in the order the kinds
-- join: `process_status` (finished background processes), then
-- `agent_message` (messages and reports from other agents). Add a source for
-- another kind with `smelt.transcript.pending:add_source(name, source)` and
-- read its blocks with `smelt.transcript.pending_blocks()`.

local bar = require("smelt._bar")
local builtins = require("smelt.transcript.builtins")

local signals = { "transcript_pending_revision" }

local function pending_of_kind(kind)
  local blocks = {}
  for _, block in ipairs(smelt.transcript.pending_blocks()) do
    if block.kind == kind then blocks[#blocks + 1] = block end
  end
  return blocks
end

-- One block reads as its transcript header; several read as their group.
local function process_status_source()
  local blocks = pending_of_kind("process_status")
  if #blocks == 0 then return nil end
  local text, hl = blocks[1].text, blocks[1].hl_group or "SmeltProcess"
  if #blocks > 1 then
    local failed
    text, failed = builtins.background_process_summary(blocks)
    hl = failed and "ErrorMsg" or "SmeltProcess"
  end
  return {
    text = text,
    style = { fg = hl, italic = true, dim = true },
    truncatable = true,
    row = 1,
  }
end

-- One message reads as its transcript header; several read as their group.
local function agent_message_source()
  local blocks = pending_of_kind("agent_message")
  if #blocks == 0 then return nil end
  local text, hl = blocks[1].text, "SmeltAgent"
  if #blocks > 1 then
    text = builtins.agent_message_summary(blocks)
  elseif blocks[1].agent_status == "failed" or blocks[1].agent_status == "blocked" then
    hl = "ErrorMsg"
  end
  return {
    text = text,
    style = { fg = hl, italic = true, dim = true },
    truncatable = true,
    row = 2,
  }
end

--- The strip of blocks that wait to join the transcript. It takes no rows
--- while nothing is pending. Its built-in sources are `process_status` and
--- `agent_message`, one row each, in the order they join.
---@tier ui_host
---@type smelt.bar.Bar
smelt.transcript.pending = bar.new({
  name = "smelt.transcript.pending",
  min_rows = 0,
  sources = {
    { "process_status", { render = process_status_source, signals = signals } },
    { "agent_message", { render = agent_message_source, signals = signals } },
  },
  win_opts = { pad_left = 0, pad_right = 1, focus_on_click = smelt.transcript.win },
})

return smelt.transcript.pending
